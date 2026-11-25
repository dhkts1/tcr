//! Anthropic's unified headers, the probe's buckets, and the standard limits.

use super::*;

#[test]
fn parses_unified_utilization_and_reset() {
    let reset_secs = 1_893_456_000i64;
    let headers = TestHeaders::new(&[
        ("anthropic-ratelimit-unified-5h-utilization", "0.42"),
        (
            "anthropic-ratelimit-unified-5h-reset",
            &reset_secs.to_string(),
        ),
        ("anthropic-ratelimit-unified-7d-utilization", "0.10"),
        ("anthropic-ratelimit-unified-status", "allowed_warning"),
    ]);
    let mut quota = Quota::default();
    assert!(
        quota.update_from_headers(&headers),
        "a response carrying the unified 5h header reports the window"
    );

    let five = quota.five_hour.unwrap();
    assert_eq!(five.utilization, 0.42);
    assert_eq!(
        five.reset,
        Some(OffsetDateTime::from_unix_timestamp(reset_secs).unwrap())
    );
    assert_eq!(quota.seven_day.unwrap().utilization, 0.10);
    assert_eq!(quota.status.as_deref(), Some("allowed_warning"));
}

/// Finding #11: a non-finite utilization header (`nan`/`inf`) must be treated
/// as absent, not parsed into a NaN that reads as under-quota (NaN >= threshold
/// is always false, so a spent account would stay in rotation). A normal numeric
/// header still parses.
#[test]
fn rejects_non_finite_utilization_headers() {
    for bad in ["nan", "inf", "-inf"] {
        let headers = TestHeaders::new(&[("anthropic-ratelimit-unified-5h-utilization", bad)]);
        assert_eq!(
            get_parsed::<f64>(&headers, "anthropic-ratelimit-unified-5h-utilization")
                .filter(|v| v.is_finite()),
            None,
            "{bad:?} must parse to None"
        );
        let mut quota = Quota::default();
        assert!(
            !quota.update_from_headers(&headers),
            "{bad:?} is not a readable window, so it must not report one either"
        );
        assert!(
            quota.five_hour.is_none(),
            "{bad:?} utilization must not create a window"
        );
    }

    let headers = TestHeaders::new(&[("anthropic-ratelimit-unified-5h-utilization", "0.87")]);
    assert_eq!(
        get_parsed::<f64>(&headers, "anthropic-ratelimit-unified-5h-utilization")
            .filter(|v| v.is_finite()),
        Some(0.87)
    );
}

/// Bug #2, proven: a window whose reset is in the PAST reads as a fresh 0.0,
/// never the stale last-learned utilization — computed live against `now`.
#[test]
fn expired_quota_window_reads_as_fresh_zero() {
    let now = OffsetDateTime::now_utc();
    let window = QuotaWindow {
        utilization: 0.95,
        reset: Some(now - Duration::hours(1)), // already reset an hour ago
    };

    // Live read past the reset is fresh, not the cached 0.95.
    assert_eq!(window.effective(now), 0.0);

    // And an account carrying only that expired window is NOT near quota,
    // so it stays eligible for selection instead of being wrongly skipped.
    let quota = Quota {
        five_hour: Some(window),
        ..Quota::default()
    };
    assert!(!quota.is_near(0.90, now));

    // Sanity: before the reset the same window still reads full.
    let before_reset = now - Duration::hours(2);
    assert_eq!(window.effective(before_reset), 0.95);
    assert!(quota.is_near(0.90, before_reset));
}

#[test]
fn is_near_true_when_five_hour_over_threshold() {
    let now = OffsetDateTime::now_utc();
    let quota = Quota {
        five_hour: Some(QuotaWindow {
            utilization: 0.95,
            reset: Some(now + Duration::hours(1)),
        }),
        ..Quota::default()
    };
    assert!(quota.is_near(0.90, now));
}

#[test]
fn governing_weekly_reset_hides_past_reset() {
    let now = OffsetDateTime::now_utc();
    let quota = Quota {
        seven_day: Some(QuotaWindow {
            utilization: 0.5,
            reset: Some(now - Duration::minutes(5)),
        }),
        ..Quota::default()
    };
    assert_eq!(quota.governing_weekly_reset(now), None);
}

/// Finding #4/#8: a high utilization learned WITHOUT a reset must not pin the
/// account out of rotation forever. `update_from_headers` synthesizes a
/// bounded reset, so the window gates now but reads fresh once the synthesized
/// window (5h) has elapsed — clearable, never permanent.
#[test]
fn resetless_high_util_window_is_bounded_not_permanent() {
    let mut quota = Quota::default();
    // Utilization with NO reset header.
    let headers = TestHeaders::new(&[("anthropic-ratelimit-unified-5h-utilization", "0.95")]);
    assert!(quota.update_from_headers(&headers));

    let now = OffsetDateTime::now_utc();
    // A reset was synthesized in the future, so it gates right now …
    assert!(
        quota.is_near(0.90, now),
        "resetless high util gates immediately"
    );
    assert!(
        quota.five_hour.unwrap().reset.is_some(),
        "a bounded reset was synthesized"
    );
    // … but clears once the synthesized 5h window has elapsed — not forever.
    let after_window = now + Duration::hours(6);
    assert!(
        !quota.is_near(0.90, after_window),
        "resetless window must clear after its bounded lifetime, never pin forever"
    );
}

/// Build a quota that a served 429 left carrying `rejected`, with `util`
/// already spent on the shared window named by `on_seven_day`.
fn rejected_with(on_seven_day: bool, util: f64) -> Quota {
    let window = Some(QuotaWindow {
        utilization: util,
        reset: Some(OffsetDateTime::now_utc() + Duration::hours(3)),
    });
    Quota {
        five_hour: if on_seven_day { None } else { window },
        seven_day: if on_seven_day { window } else { None },
        status: Some("rejected".to_string()),
        ..Default::default()
    }
}

fn bucket(util: f64) -> Option<crate::records::probe::UsageBucket> {
    Some(crate::records::probe::UsageBucket {
        utilization: Some(util),
        reset_at_ms: None,
    })
}

/// The bug this exists to end: `rejected` outliving the window that caused it.
///
/// `update_from_headers` is the only writer of `status` and it needs a SERVED
/// response, but `account_terminal_gate` skips a rejected account, so no served
/// response ever arrives. Before this, the probe moved the bars and left
/// `status` alone, and the account stayed benched until a restart.
#[test]
fn probed_drop_in_utilization_clears_a_stale_rejection() {
    for on_seven_day in [false, true] {
        let mut quota = rejected_with(on_seven_day, 0.99);
        let usage = if on_seven_day {
            crate::records::probe::Usage {
                seven_day: bucket(0.04),
                ..Default::default()
            }
        } else {
            crate::records::probe::Usage {
                five_hour: bucket(0.04).into(),
                ..Default::default()
            }
        };

        quota.apply_usage(&usage);

        assert_eq!(
            quota.status, None,
            "a utilization that FELL is upstream starting a new window, so the \
             rejection recorded against the old one is stale (seven_day={on_seven_day})"
        );
    }
}

/// The other side of the same rule: a rejection that is still current must
/// survive the probe, or every probe cadence would put a genuinely blocked
/// account back into rotation to earn another 429.
#[test]
fn probe_leaves_a_live_rejection_alone() {
    // Utilization climbing within the same window.
    let mut climbing = rejected_with(false, 0.97);
    climbing.apply_usage(&crate::records::probe::Usage {
        five_hour: bucket(0.99).into(),
        ..Default::default()
    });
    assert_eq!(
        climbing.status.as_deref(),
        Some("rejected"),
        "a RISING utilization is the same window still running"
    );

    // Identical reading: no evidence of anything.
    let mut flat = rejected_with(false, 0.99);
    flat.apply_usage(&crate::records::probe::Usage {
        five_hour: bucket(0.99).into(),
        ..Default::default()
    });
    assert_eq!(
        flat.status.as_deref(),
        Some("rejected"),
        "an unchanged utilization is not evidence of a roll"
    );

    // The probe read nothing for this window.
    let mut silent = rejected_with(false, 0.99);
    silent.apply_usage(&crate::records::probe::Usage::default());
    assert_eq!(
        silent.status.as_deref(),
        Some("rejected"),
        "silence is not evidence of a roll"
    );

    // We never stored a prior reading, so there is nothing to compare against.
    let mut unknown = Quota {
        status: Some("rejected".to_string()),
        ..Default::default()
    };
    unknown.apply_usage(&crate::records::probe::Usage {
        five_hour: bucket(0.01).into(),
        ..Default::default()
    });
    assert_eq!(
        unknown.status.as_deref(),
        Some("rejected"),
        "with no stored utilization a low reading proves nothing"
    );
}

/// `seven_day_oi` is the model-scoped (Fable) weekly. A Fable-only rejection
/// never reaches `status` in the first place (`update_from_headers`), so a roll
/// on that window is not evidence about an account-wide rejection and must not
/// clear one.
#[test]
fn a_fable_weekly_roll_does_not_clear_an_account_wide_rejection() {
    let mut quota = Quota {
        seven_day_oi: Some(QuotaWindow {
            utilization: 0.99,
            reset: Some(OffsetDateTime::now_utc() + Duration::hours(3)),
        }),
        status: Some("rejected".to_string()),
        ..Default::default()
    };

    quota.apply_usage(&crate::records::probe::Usage {
        seven_day_oi: bucket(0.02),
        ..Default::default()
    });

    assert_eq!(
        quota.status.as_deref(),
        Some("rejected"),
        "the model-scoped weekly cannot be the cause of an account-wide rejection"
    );
}

/// The clear is scoped to `rejected`. `allowed_warning` carries the near-limit
/// signal the TUI colours on, and a rolled window must not silently erase it.
#[test]
fn a_rolled_window_does_not_erase_a_non_rejected_status() {
    let mut quota = Quota {
        status: Some("allowed_warning".to_string()),
        ..rejected_with(false, 0.99)
    };

    quota.apply_usage(&crate::records::probe::Usage {
        five_hour: bucket(0.01).into(),
        ..Default::default()
    });

    assert_eq!(
        quota.status.as_deref(),
        Some("allowed_warning"),
        "only a rejection is dropped here"
    );
}

/// The zero-spend probe must NOT fabricate a 5h window on an idle account.
///
/// Measured live 2026-07-31: every probe sweep stamped idle accounts with a
/// reset exactly `now + 5h`, and `warm_targets` reads a live 5h reset as
/// "already warm", so keep-warm had 0 eligible targets of 13 while 7
/// accounts carried a future reset at 0.00 utilization. The probe starts no
/// window — only a served request does — so a zero bucket must leave the
/// reset `None`.
#[test]
fn probed_zero_bucket_does_not_fabricate_a_window() {
    let mut quota = Quota::default();
    quota.apply_usage(&crate::records::probe::Usage {
        five_hour: crate::records::probe::WindowReading::Read(crate::records::probe::UsageBucket {
            utilization: Some(0.0),
            reset_at_ms: None,
        }),
        ..Default::default()
    });

    let now = OffsetDateTime::now_utc();
    let window = quota.five_hour.expect("the bucket was applied");
    assert_eq!(
        window.reset, None,
        "a zero-utilization probe must not synthesize a reset"
    );
    assert_eq!(
        window.live_reset(now),
        None,
        "and must therefore not read as a live window to warm_targets"
    );
}

/// The guard above must not weaken bug #4/#5 on the path it was written for:
/// a NON-zero probed utilization with no reset still gets a bounded window,
/// so it gates now and clears later rather than pinning the account forever.
#[test]
fn probed_nonzero_bucket_without_reset_still_gets_a_bounded_window() {
    let mut quota = Quota::default();
    quota.apply_usage(&crate::records::probe::Usage {
        five_hour: crate::records::probe::WindowReading::Read(crate::records::probe::UsageBucket {
            utilization: Some(0.95),
            reset_at_ms: None,
        }),
        ..Default::default()
    });

    let now = OffsetDateTime::now_utc();
    assert!(
        quota.five_hour.expect("applied").reset.is_some(),
        "a resetless HIGH utilization still needs its bounded reset"
    );
    assert!(quota.is_near(0.90, now), "and gates immediately");
    assert!(
        !quota.is_near(0.90, now + Duration::hours(6)),
        "clearing after one bounded window, never pinning forever"
    );
}

/// A bucket the endpoint mentions but whose utilization does not parse, with
/// no prior reading, must stay ABSENT — not be committed as a fabricated
/// `0.0`. A fabricated zero carries the bucket's reset with it, which reads to
/// `warm_targets` as "already warm — skip" and to `governing_weekly_reset` as
/// a known window; and it renders as a confident `0%` bar. Absent reaches the
/// wire as `null` and renders as "unmeasured".
#[test]
fn unreadable_utilization_with_no_prior_stays_absent() {
    let mut quota = Quota::default();
    quota.apply_usage(&crate::records::probe::Usage {
        five_hour: crate::records::probe::WindowReading::Read(crate::records::probe::UsageBucket {
            utilization: None,
            reset_at_ms: Some(crate::now_ms() + 3_600_000),
        }),
        ..Default::default()
    });

    assert!(
        quota.five_hour.is_none(),
        "an unreadable utilization with no prior must not fabricate a window"
    );
    // And the reported reset goes with it: a reset with no measurement behind
    // it would read as "already warm" to `warm_targets` and suppress keep-warm.
    let now = OffsetDateTime::now_utc();
    assert_eq!(
        quota.five_hour.and_then(|w| w.live_reset(now)),
        None,
        "no window means no live reset to suppress keep-warm with"
    );
    // Nothing to report upward — this is the `None` the snapshot maps to JSON
    // `null`. `max_utilization` reads the same either way (an absent window and
    // a `Some(0.0)` both leave the fold at `0.0`); the difference is on the
    // wire and in the reset-derived signals asserted above.
    assert_eq!(quota.max_utilization(now, false), 0.0);
}

/// The same unreadable bucket must still PRESERVE a prior reading — that half
/// of the merge is not what was broken.
///
/// The probe here reports a NEW reset alongside the unreadable utilization,
/// which is what makes this test able to fail: skipping the merge entirely on
/// an unreadable utilization would also preserve the prior utilization (by not
/// writing at all) and would be indistinguishable — except that the freshly
/// reported reset would be dropped on the floor.
#[test]
fn unreadable_utilization_preserves_a_prior_reading() {
    let now = OffsetDateTime::now_utc();
    let prior_reset = now + Duration::hours(3);
    let reported_reset = now + Duration::hours(4);
    let mut quota = Quota {
        five_hour: Some(QuotaWindow {
            utilization: 0.77,
            reset: Some(prior_reset),
        }),
        ..Quota::default()
    };
    quota.apply_usage(&crate::records::probe::Usage {
        five_hour: crate::records::probe::WindowReading::Read(crate::records::probe::UsageBucket {
            utilization: None,
            reset_at_ms: Some((reported_reset.unix_timestamp_nanos() / 1_000_000) as i64),
        }),
        ..Default::default()
    });

    let window = quota.five_hour.expect("the prior window must survive");
    assert_eq!(window.utilization, 0.77, "prior utilization kept");
    assert_eq!(
        window.reset.map(OffsetDateTime::unix_timestamp),
        Some(reported_reset.unix_timestamp()),
        "and the freshly reported reset is still adopted"
    );
    // With no reported reset the still-future prior reset is inherited instead.
    let mut quota = Quota {
        five_hour: Some(QuotaWindow {
            utilization: 0.77,
            reset: Some(prior_reset),
        }),
        ..Quota::default()
    };
    quota.apply_usage(&crate::records::probe::Usage {
        five_hour: crate::records::probe::WindowReading::Read(crate::records::probe::UsageBucket {
            utilization: None,
            reset_at_ms: None,
        }),
        ..Default::default()
    });
    assert_eq!(
        quota.five_hour.map(|w| (w.utilization, w.reset)),
        Some((0.77, Some(prior_reset))),
        "prior utilization and prior reset both intact"
    );
}

/// The guard above must not swing the error the other way: a GENUINE numeric
/// zero is a reading, and must stay a present `0.0` window rather than
/// collapsing into "unmeasured". This is the test that pins the two apart.
#[test]
fn a_genuine_zero_utilization_is_a_reading_not_an_absence() {
    let mut quota = Quota::default();
    quota.apply_usage(&crate::records::probe::Usage {
        five_hour: crate::records::probe::WindowReading::Read(crate::records::probe::UsageBucket {
            utilization: Some(0.0),
            reset_at_ms: None,
        }),
        ..Default::default()
    });

    let window = quota
        .five_hour
        .expect("a genuine 0% is a measurement — the window must be present");
    assert_eq!(window.utilization, 0.0);
    // A later unreadable probe then falls into the prior-preserving arm, so
    // the measured zero survives instead of blanking the bar.
    quota.apply_usage(&crate::records::probe::Usage {
        five_hour: crate::records::probe::WindowReading::Read(crate::records::probe::UsageBucket {
            utilization: None,
            reset_at_ms: None,
        }),
        ..Default::default()
    });
    assert_eq!(
        quota.five_hour.map(|w| w.utilization),
        Some(0.0),
        "a measured zero is not re-classified as unmeasured by later silence"
    );
}

/// Finding #3: a spent standard (API-key) limit must read fresh once its
/// `standard_reset` has passed, or the account is pinned out permanently
/// (no OAuth probe re-reads API-key limits).
#[test]
fn standard_limit_reads_fresh_after_reset() {
    let now = OffsetDateTime::now_utc();
    let spent = Quota {
        tokens_limit: Some(1000),
        tokens_remaining: Some(0),
        standard_reset: Some(now - Duration::hours(1)),
        ..Quota::default()
    };
    assert!(
        !spent.is_near(0.90, now),
        "a standard window past its reset must not keep gating"
    );

    // Before the reset the same spent window still gates.
    let unexpired = Quota {
        standard_reset: Some(now + Duration::hours(1)),
        ..spent
    };
    assert!(unexpired.is_near(0.90, now));
}

/// Per-model routing: `model_weekly_exhausted` gates on the model-scoped
/// `7d_oi` (Fable) bucket. True when the bucket is at/over threshold with a
/// future reset; false when the bucket was never learned.
#[test]
fn model_weekly_exhausted_true_when_oi_over_threshold() {
    let now = OffsetDateTime::now_utc();
    let quota = Quota {
        seven_day_oi: Some(QuotaWindow {
            utilization: 0.95,
            reset: Some(now + Duration::hours(2)),
        }),
        ..Quota::default()
    };
    assert!(quota.model_weekly_exhausted(0.90, now));
}

#[test]
fn model_weekly_exhausted_false_when_oi_absent() {
    let now = OffsetDateTime::now_utc();
    assert!(!Quota::default().model_weekly_exhausted(0.90, now));
}

/// Bug #2 applies here too: a `7d_oi` bucket past its reset reads as a fresh
/// `0.0`, so an exhausted-then-reset Fable bucket must NOT keep gating.
#[test]
fn model_weekly_exhausted_reads_fresh_after_reset() {
    let now = OffsetDateTime::now_utc();
    let quota = Quota {
        seven_day_oi: Some(QuotaWindow {
            utilization: 0.95,
            reset: Some(now - Duration::hours(1)), // reset already passed
        }),
        ..Quota::default()
    };
    assert!(!quota.model_weekly_exhausted(0.90, now));
}

/// Fable weekly gate (three-tier): `model_weekly_rejected` is the AUTHORITY
/// check, distinct from the PREDICTION `model_weekly_exhausted` — a bucket
/// sitting at 0.96 (over a 0.95 threshold, the observed live-fleet shape)
/// is exhausted-by-prediction but NOT upstream-rejected.
#[test]
fn model_weekly_rejected_false_when_only_over_threshold() {
    let now = OffsetDateTime::now_utc();
    let quota = Quota {
        seven_day_oi: Some(QuotaWindow {
            utilization: 0.96,
            reset: Some(now + Duration::hours(2)),
        }),
        ..Quota::default()
    };
    assert!(
        quota.model_weekly_exhausted(0.95, now),
        "0.96 is over the 0.95 threshold — predicted exhausted"
    );
    assert!(
        !quota.model_weekly_rejected(now),
        "0.96 is not 1.0 — upstream has not actually rejected"
    );
}

#[test]
fn model_weekly_rejected_true_only_at_or_over_1_0() {
    let now = OffsetDateTime::now_utc();
    let quota = Quota {
        seven_day_oi: Some(QuotaWindow {
            utilization: 1.0,
            reset: Some(now + Duration::hours(2)),
        }),
        ..Quota::default()
    };
    assert!(quota.model_weekly_rejected(now));
}

#[test]
fn model_weekly_rejected_false_when_oi_absent() {
    let now = OffsetDateTime::now_utc();
    assert!(!Quota::default().model_weekly_rejected(now));
}

/// A `7d_oi` rejection past its reset must read fresh (`0.0`), the same
/// bug-#2 shape `model_weekly_exhausted_reads_fresh_after_reset` already
/// guards for the prediction side.
#[test]
fn model_weekly_rejected_reads_fresh_after_reset() {
    let now = OffsetDateTime::now_utc();
    let quota = Quota {
        seven_day_oi: Some(QuotaWindow {
            utilization: 1.0,
            reset: Some(now - Duration::hours(1)),
        }),
        ..Quota::default()
    };
    assert!(!quota.model_weekly_rejected(now));
}

/// Regression guard: `is_near` must keep IGNORING the `7d_oi` bucket, so a
/// full Fable weekly never gates the shared (non-Fable) rotation. Only the
/// new `model_weekly_exhausted` — consulted for Fable requests alone — sees it.
#[test]
fn is_near_still_ignores_seven_day_oi() {
    let now = OffsetDateTime::now_utc();
    let quota = Quota {
        seven_day_oi: Some(QuotaWindow {
            utilization: 0.95,
            reset: Some(now + Duration::hours(2)),
        }),
        ..Quota::default()
    };
    assert!(!quota.is_near(0.90, now));
}

/// The exact header block from issue #178, applied the way a 429 applies it:
/// the account must come out blocked for Fable and eligible for everything
/// else. Both halves are asserted, because the bug was that the shared
/// windows went down with the model-scoped one.
#[test]
fn the_issue_178_header_block_blocks_only_fable() {
    let now = OffsetDateTime::now_utc();
    let headers = TestHeaders::new(&[
        ("anthropic-ratelimit-unified-status", "rejected"),
        ("anthropic-ratelimit-unified-5h-status", "allowed"),
        ("anthropic-ratelimit-unified-7d-status", "allowed_warning"),
        ("anthropic-ratelimit-unified-7d_oi-status", "rejected"),
    ]);
    let mut quota = Quota::default();
    assert!(!quota.update_from_headers(&headers));
    quota.reject_model_weekly(&headers);

    assert!(
        quota.model_weekly_exhausted(0.90, now),
        "the Fable weekly scope must read exhausted"
    );
    assert!(
        !quota.is_near(0.90, now),
        "the shared windows must stay allowed, so Opus and Sonnet still route here"
    );
    assert_ne!(
        quota.status.as_deref(),
        Some("rejected"),
        "the aggregate must not be stored account-wide when a sub-scope explains it \
         — `Manager::account_terminal_gate` reads it as a terminal gate"
    );
}

/// Step 5/6 of issue #178's repro: a later response whose `7d_oi` scope reads
/// allowed clears the rejection. The window itself must survive — dropping it
/// would lose the `reset` that the exhausted-fleet response reports as "frees
/// at", which is the regression F-04 of the branch review named.
#[test]
fn allowed_seven_day_oi_status_clears_the_rejection_without_nulling_the_window() {
    let now = OffsetDateTime::now_utc();
    let reset_secs = (now + Duration::days(3)).unix_timestamp();
    let mut quota = Quota::default();
    quota.reject_model_weekly(&TestHeaders::new(&[(
        "anthropic-ratelimit-unified-7d_oi-reset",
        &reset_secs.to_string(),
    )]));
    assert!(quota.model_weekly_exhausted(0.90, now));

    let allowed = TestHeaders::new(&[
        ("anthropic-ratelimit-unified-status", "allowed"),
        ("anthropic-ratelimit-unified-5h-status", "allowed"),
        ("anthropic-ratelimit-unified-7d-status", "allowed"),
        ("anthropic-ratelimit-unified-7d_oi-status", "allowed"),
    ]);
    assert!(!quota.update_from_headers(&allowed));

    assert!(
        !quota.model_weekly_exhausted(0.90, now),
        "an allowed verdict on this scope must free Fable again"
    );
    let window = quota
        .seven_day_oi
        .expect("the learned window must survive the recovery, reset and all");
    assert_eq!(window.utilization, 0.0);
    assert_eq!(
        window.reset.map(OffsetDateTime::unix_timestamp),
        Some(reset_secs),
        "the known reset must be inherited, not re-synthesized"
    );
}

/// The stale `fiveHour=0.0` rows of 2026-11-01: a 5h window tracked from before
/// the plan lost it, then a probe saying `"five_hour": null`. The null clears the
/// window and marks the plan; a later probe with no 5h key changes neither; a
/// later 5h header (the plan evidently has the window again) clears the mark.
#[test]
fn an_explicit_five_hour_null_clears_the_window_and_marks_the_plan() {
    let now = OffsetDateTime::from_unix_timestamp(1_900_000_000).unwrap();
    let mut quota = Quota {
        five_hour: Some(QuotaWindow {
            utilization: 0.4,
            reset: Some(now + Duration::hours(2)),
        }),
        ..Default::default()
    };

    quota.apply_usage_at(
        &crate::records::probe::Usage {
            five_hour: crate::records::probe::WindowReading::PlanHasNone,
            seven_day: bucket(0.1),
            ..Default::default()
        },
        now,
    );
    assert!(
        quota.five_hour.is_none(),
        "a plan with no 5h window keeps no 5h window: {:?}",
        quota.five_hour
    );
    assert!(quota.five_hour_absent_on_plan);
    assert!(quota.seven_day.is_some(), "the weekly still folds");

    quota.apply_usage_at(&crate::records::probe::Usage::default(), now);
    assert!(
        quota.five_hour_absent_on_plan && quota.five_hour.is_none(),
        "an unread 5h is not evidence either way"
    );

    let headers = TestHeaders::new(&[
        ("anthropic-ratelimit-unified-5h-utilization", "0.05"),
        ("anthropic-ratelimit-unified-5h-reset", "1900018000"),
    ]);
    assert!(quota.update_from_headers_at(&headers, now));
    assert!(
        !quota.five_hour_absent_on_plan,
        "a 5h header is the plan having the window"
    );
    assert_eq!(quota.five_hour.map(|w| w.utilization), Some(0.05));
}

/// A 5h bucket read by the probe clears the mark too, and folds as it always did.
#[test]
fn a_five_hour_bucket_after_a_null_clears_the_mark() {
    let now = OffsetDateTime::from_unix_timestamp(1_900_000_000).unwrap();
    let mut quota = Quota {
        five_hour_absent_on_plan: true,
        ..Default::default()
    };
    quota.apply_usage_at(
        &crate::records::probe::Usage {
            five_hour: bucket(0.3).into(),
            ..Default::default()
        },
        now,
    );
    assert!(!quota.five_hour_absent_on_plan);
    assert_eq!(quota.five_hour.map(|w| w.utilization), Some(0.3));
}
