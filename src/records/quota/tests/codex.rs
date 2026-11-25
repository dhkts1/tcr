//! Codex: the `x-codex-*` headers, refusals and holds, and the facts beside the windows.

use super::*;

#[test]
fn a_header_quota_refusal_can_read_reset_from_a_body_without_a_limit_type() {
    let now = OffsetDateTime::from_unix_timestamp(1_900_000_000).unwrap();
    let headers = TestHeaders::new(&[(CODEX_REACHED_TYPE_HEADER, "primary")]);
    let body = br#"{"error":{"message":"out","resets_at":1900007200}}"#;
    let refusal = CodexQuotaRefusal::parse(&headers, body, now).expect("header quota refusal");
    assert_eq!(
        refusal.reset.map(OffsetDateTime::unix_timestamp),
        Some(1_900_007_200)
    );
}

#[test]
fn a_codex_quota_refusal_stores_its_body_reset_in_the_quota_snapshot() {
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let reset = OffsetDateTime::from_unix_timestamp(now + 7200).unwrap();
    let body = serde_json::json!({
        "error": {"type": "usage_limit_reached", "resets_at": reset.unix_timestamp()}
    })
    .to_string();

    // Each with whether it carries the five-hour window, which is what the fold returns.
    for (headers, carries_five_hour) in [
        (TestHeaders::new(&[]), false),
        (codex_headers("100", "100", now + 1800, now + 86_400), true),
    ] {
        let mut quota = Quota::default();

        let refusal =
            CodexQuotaRefusal::parse(&headers, body.as_bytes(), OffsetDateTime::now_utc())
                .expect("body quota refusal");
        assert_eq!(
            quota.update_from_codex_refusal(&headers, 429, Some(&refusal)),
            carries_five_hour
        );

        assert_eq!(quota.status.as_deref(), Some("rejected"));
        assert_eq!(
            quota.codex.refusal_reset,
            Some(reset),
            "the body deadline reaches the snapshot and outranks header windows"
        );
    }
}

// --- Codex: the `x-codex-*` quota headers --------------------------------

/// A Codex response's six window headers as `openai/codex` names them
/// (`codex-api/src/rate_limits.rs:68-79`): the primary window 300 minutes and
/// the secondary 10 080, tcr's five-hour and weekly lengths. Used values are
/// percents (0–100); resets are epoch seconds.
fn codex_headers(
    primary_used: &str,
    secondary_used: &str,
    reset_primary: i64,
    reset_secondary: i64,
) -> TestHeaders {
    TestHeaders::new(&[
        ("x-codex-primary-used-percent", primary_used),
        ("x-codex-primary-window-minutes", "300"),
        ("x-codex-primary-reset-at", &reset_primary.to_string()),
        ("x-codex-secondary-used-percent", secondary_used),
        ("x-codex-secondary-window-minutes", "10080"),
        ("x-codex-secondary-reset-at", &reset_secondary.to_string()),
    ])
}

/// The two-window shape the source describes: each percent becomes the
/// fraction every threshold comparison already speaks (25 → 0.25), each reset
/// is taken as reported, the model-scoped bucket stays empty, and a 200 reads
/// as `allowed`.
#[test]
fn codex_headers_fill_the_five_hour_and_weekly_windows_as_fractions() {
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let mut q = Quota::default();
    let read = q.update_from_codex_refusal(
        &codex_headers("25", "3.5", now + 3600, now + 86400),
        200,
        None,
    );
    assert!(read, "a primary window was read");
    let fh = q.five_hour.expect("five_hour");
    assert!(
        (fh.utilization - 0.25).abs() < 1e-9,
        "25 percent is 0.25, got {}",
        fh.utilization
    );
    assert_eq!(fh.reset.unwrap().unix_timestamp(), now + 3600);
    let sd = q.seven_day.expect("seven_day");
    assert!(
        (sd.utilization - 0.035).abs() < 1e-9,
        "3.5 percent is 0.035, got {}",
        sd.utilization
    );
    assert_eq!(sd.reset.unwrap().unix_timestamp(), now + 86400);
    assert!(
        q.seven_day_oi.is_none(),
        "Codex has no model-scoped weekly bucket"
    );
    assert_eq!(q.status.as_deref(), Some("allowed"));
}

/// `updated_at` (`quotaUpdatedAtMs` on the wire) moves on a Codex account's own
/// readings, by the rule `update_from_headers` applies to an Anthropic one: a
/// response that carried a Codex quota fact that was read stamps it, and one
/// that carried none leaves it alone. A weekly-only reading counts though it
/// returns `false` (it is the only window the measured plan reports), and so
/// does a quota refusal, the Codex twin of a `unified-status` verdict. A
/// response with no Codex header, a 0% placeholder slot with no length or
/// reset, and a 429 that is not the quota read nothing.
///
/// Watched red: without the stamp in `update_from_codex_refusal_at`, the
/// weekly-only reading leaves `updated_at` at `None`.
#[test]
fn a_codex_reading_stamps_updated_at_and_silence_does_not() {
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let stale = OffsetDateTime::from_unix_timestamp(now - 40 * 60).unwrap();
    let mut q = Quota::default();

    assert!(!q.update_from_codex_refusal(&TestHeaders::new(&[]), 200, None));
    assert_eq!(q.updated_at, None, "no Codex header is no reading");

    let weekly_only = TestHeaders::new(&[
        ("x-codex-primary-used-percent", "40"),
        ("x-codex-primary-window-minutes", "10080"),
        ("x-codex-primary-reset-at", &(now + 86_400).to_string()),
    ]);
    let before = OffsetDateTime::now_utc();
    assert!(
        !q.update_from_codex_refusal(&weekly_only, 200, None),
        "control: a weekly-only reading returns false"
    );
    let stamped = q
        .updated_at
        .expect("a weekly-only Codex reading is still a reading");
    assert!(stamped >= before, "{stamped} predates the reading");

    for (label, silence, status) in [
        ("no header", TestHeaders::new(&[]), 502),
        (
            "a 0% placeholder slot",
            TestHeaders::new(&[("x-codex-secondary-used-percent", "0")]),
            200,
        ),
        (
            "a 429 that is not the quota",
            TestHeaders::new(&[("retry-after", "60")]),
            429,
        ),
    ] {
        q.updated_at = Some(stale);
        assert!(
            !q.update_from_codex_refusal(
                &silence,
                status,
                (status == 429)
                    .then(|| CodexQuotaRefusal::parse(&silence, &[], OffsetDateTime::now_utc()))
                    .flatten()
                    .as_ref(),
            )
        );
        assert_eq!(q.updated_at, Some(stale), "{label} must not move it");
    }

    q.updated_at = Some(stale);
    let before = OffsetDateTime::now_utc();
    let refusal = TestHeaders::new(&[(CODEX_REACHED_TYPE_HEADER, "primary")]);
    assert!(!q.update_from_codex_refusal(
        &refusal,
        429,
        CodexQuotaRefusal::parse(&refusal, &[], OffsetDateTime::now_utc()).as_ref(),
    ));
    assert_eq!(
        q.status.as_deref(),
        Some("rejected"),
        "control: read as the quota"
    );
    assert!(
        q.updated_at.is_some_and(|at| at >= before),
        "a quota refusal is a reading: {:?}",
        q.updated_at
    );
}

/// The mapping is by LENGTH: a plan that lists its weekly window first still
/// lands each number in the right bucket, and the five-hour read is reported
/// from whichever slot carried it.
#[test]
fn codex_windows_are_mapped_by_length_not_position() {
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let swapped = TestHeaders::new(&[
        ("x-codex-primary-used-percent", "40"),
        ("x-codex-primary-window-minutes", "10080"),
        ("x-codex-primary-reset-at", &(now + 86400).to_string()),
        ("x-codex-secondary-used-percent", "10"),
        ("x-codex-secondary-window-minutes", "300"),
        ("x-codex-secondary-reset-at", &(now + 600).to_string()),
    ]);
    let mut q = Quota::default();
    assert!(
        q.update_from_codex_refusal(&swapped, 200, None),
        "the five-hour window was read, from the secondary slot"
    );
    assert!((q.five_hour.unwrap().utilization - 0.10).abs() < 1e-9);
    assert!((q.seven_day.unwrap().utilization - 0.40).abs() < 1e-9);
}

/// With no `window-minutes` the slot decides: primary is the five-hour window,
/// secondary the weekly. A window reported without its reset gets the bounded
/// one [`resolve_reset`] synthesizes, never none (bugs #4/#5), and of ITS
/// bucket's length: five hours for the primary slot, seven days for the
/// secondary. Swapping the two lengths in the parser fails the reset checks.
#[test]
fn codex_windows_without_minutes_fall_back_to_position() {
    let only_pct = TestHeaders::new(&[
        ("x-codex-primary-used-percent", "50"),
        ("x-codex-secondary-used-percent", "5"),
    ]);
    let mut q = Quota::default();
    let before = OffsetDateTime::now_utc();
    assert!(
        q.update_from_codex_refusal(&only_pct, 200, None),
        "by position the primary slot is the five-hour window"
    );
    let after = OffsetDateTime::now_utc();
    assert!((q.five_hour.unwrap().utilization - 0.5).abs() < 1e-9);
    assert!((q.seven_day.unwrap().utilization - 0.05).abs() < 1e-9);
    let five_hour_reset = q.five_hour.unwrap().reset.expect("a synthesized reset");
    assert!(
        five_hour_reset >= before + FIVE_HOUR && five_hour_reset <= after + FIVE_HOUR,
        "the five-hour window resets one five-hour length out, got {five_hour_reset}"
    );
    let weekly_reset = q.seven_day.unwrap().reset.expect("a synthesized reset");
    assert!(
        weekly_reset >= before + SEVEN_DAY && weekly_reset <= after + SEVEN_DAY,
        "the weekly window resets one seven-day length out, got {weekly_reset}"
    );
}

/// The shape the backend may send for an absent window, which no inference
/// has measured yet: a `0` in the secondary slot beside the real weekly window
/// in the primary slot. Placeholder or not, it carries no data, so it neither
/// overwrites the weekly reading (97% must not become 0%) nor fabricates a
/// five-hour window (a `window-minutes: 0` is no length, never a short one).
/// Run once with `window-minutes: 0` and once with no length at all.
///
/// Then the same placeholder in the PRIMARY slot, beside the weekly window
/// read by its length from the secondary slot. That is the case only the
/// no-data skip decides: with no length the primary slot falls to the
/// five-hour bucket by position, where no merge rescues it, and nothing else
/// stops it.
///
/// Watched red with the length rule, the no-data skip and the same-bucket
/// merge all dropped: the first run fabricates a 0% five-hour window and
/// answers `true`, and the second, run alone, overwrites the weekly 0.97 with
/// 0.0. Watched red with the no-data skip ALONE dropped: the primary-slot
/// case fabricates a 0% five-hour window and answers `true`, while both
/// secondary-slot runs still pass.
#[test]
fn a_zero_codex_placeholder_slot_neither_overwrites_nor_fabricates_a_window() {
    let secs = OffsetDateTime::now_utc().unix_timestamp();
    for placeholder_minutes in [Some("0"), None] {
        let mut h = TestHeaders::new(&[
            ("x-codex-primary-used-percent", "97"),
            ("x-codex-primary-window-minutes", "10080"),
            ("x-codex-primary-reset-at", &(secs + 86_400).to_string()),
            ("x-codex-secondary-used-percent", "0"),
        ]);
        if let Some(minutes) = placeholder_minutes {
            h.push("x-codex-secondary-window-minutes", minutes);
        }
        let mut q = Quota::default();
        assert!(
            !q.update_from_codex_refusal(&h, 200, None),
            "no five-hour window was read (window-minutes {placeholder_minutes:?})"
        );
        assert!(
            q.five_hour.is_none(),
            "no five-hour window is fabricated (window-minutes {placeholder_minutes:?})"
        );
        let weekly = q.seven_day.expect("the weekly window");
        assert!(
            (weekly.utilization - 0.97).abs() < 1e-9,
            "the weekly reading stays 0.97 (window-minutes {placeholder_minutes:?}), got {}",
            weekly.utilization
        );
        assert_eq!(weekly.reset.unwrap().unix_timestamp(), secs + 86_400);
    }

    let primary_placeholder = TestHeaders::new(&[
        ("x-codex-primary-used-percent", "0"),
        ("x-codex-secondary-used-percent", "97"),
        ("x-codex-secondary-window-minutes", "10080"),
        ("x-codex-secondary-reset-at", &(secs + 86_400).to_string()),
    ]);
    let mut q = Quota::default();
    assert!(
        !q.update_from_codex_refusal(&primary_placeholder, 200, None),
        "no five-hour window was read from a primary slot that carries no data"
    );
    assert!(
        q.five_hour.is_none(),
        "a primary slot with no data fabricates no five-hour window by position"
    );
    let weekly = q
        .seven_day
        .expect("the weekly window, read from the secondary slot");
    assert!(
        (weekly.utilization - 0.97).abs() < 1e-9,
        "the weekly reading stays 0.97, got {}",
        weekly.utilization
    );
    assert_eq!(weekly.reset.unwrap().unix_timestamp(), secs + 86_400);
}

/// Two slots that land in ONE bucket keep the fuller reading and the later
/// reset, in either order: two weekly-length windows (10 080 and 43 200
/// minutes), and a weekly window beside a positional one. The last slot
/// written used to win, so the order of the headers decided which reading
/// survived.
///
/// Watched red: let each slot overwrite its bucket again and the fuller
/// reading is lost whenever it is listed first.
#[test]
fn two_codex_slots_in_one_bucket_keep_the_fuller_reading_and_the_later_reset() {
    let secs = OffsetDateTime::now_utc().unix_timestamp();
    let (early, late) = (secs + 86_400, secs + 200_000);
    for (first, second) in [
        (("90", "10080", late), ("40", "43200", early)),
        (("40", "10080", early), ("90", "43200", late)),
    ] {
        let h = TestHeaders::new(&[
            ("x-codex-primary-used-percent", first.0),
            ("x-codex-primary-window-minutes", first.1),
            ("x-codex-primary-reset-at", &first.2.to_string()),
            ("x-codex-secondary-used-percent", second.0),
            ("x-codex-secondary-window-minutes", second.1),
            ("x-codex-secondary-reset-at", &second.2.to_string()),
        ]);
        let mut q = Quota::default();
        assert!(!q.update_from_codex_refusal(&h, 200, None));
        let weekly = q.seven_day.expect("the weekly bucket");
        assert!(
            (weekly.utilization - 0.90).abs() < 1e-9,
            "the fuller reading survives, got {}",
            weekly.utilization
        );
        assert_eq!(weekly.reset.unwrap().unix_timestamp(), late);
        assert!(q.five_hour.is_none());
    }

    // A length-mapped weekly window, then a positional one (no length) in the
    // secondary slot: the positional reading may not overwrite the fuller one.
    let h = TestHeaders::new(&[
        ("x-codex-primary-used-percent", "97"),
        ("x-codex-primary-window-minutes", "10080"),
        ("x-codex-primary-reset-at", &late.to_string()),
        ("x-codex-secondary-used-percent", "5"),
        ("x-codex-secondary-reset-at", &early.to_string()),
    ]);
    let mut q = Quota::default();
    assert!(!q.update_from_codex_refusal(&h, 200, None));
    let weekly = q.seven_day.expect("the weekly bucket");
    assert!(
        (weekly.utilization - 0.97).abs() < 1e-9,
        "got {}",
        weekly.utilization
    );
    assert_eq!(weekly.reset.unwrap().unix_timestamp(), late);
}

/// A Codex 429 WITHOUT `x-codex-rate-limit-reached-type` is not a quota
/// refusal, so it leaves `status` exactly as it was: still `allowed` after a
/// 200, still unset on a fresh account. Marking it `rejected` would put a
/// transient 429 behind a terminal gate nothing clears.
///
/// Watched red: drop the reached-type check from the `rejected` arm.
#[test]
fn a_codex_429_without_a_reached_type_leaves_status_alone() {
    let secs = OffsetDateTime::now_utc().unix_timestamp();
    let h = codex_headers("30", "100", secs + 600, secs + 50_000);
    let mut q = Quota::default();
    assert!(q.update_from_codex_refusal(&h, 200, None));
    assert_eq!(q.status.as_deref(), Some("allowed"), "precondition");
    assert!(q.update_from_codex_refusal(
        &h,
        429,
        CodexQuotaRefusal::parse(&h, &[], OffsetDateTime::now_utc()).as_ref(),
    ));
    assert_eq!(
        q.status.as_deref(),
        Some("allowed"),
        "a 429 that is not a quota refusal leaves the status as it was"
    );
    let mut fresh = Quota::default();
    assert!(fresh.update_from_codex_refusal(
        &h,
        429,
        CodexQuotaRefusal::parse(&h, &[], OffsetDateTime::now_utc()).as_ref(),
    ));
    assert_eq!(fresh.status, None, "and sets none on a fresh account");
}

/// Both windows spent: the account serves again only when BOTH have rolled,
/// so the hold runs to the later reset, whichever slot lists it.
///
/// Watched red: let the first full window win again and the primary slot's
/// earlier reset holds the account for ten minutes instead of the weekly's.
#[test]
fn codex_hold_takes_the_later_reset_when_both_windows_are_full() {
    let now = OffsetDateTime::now_utc();
    let secs = now.unix_timestamp();
    for (primary_reset, secondary_reset) in
        [(secs + 600, secs + 50_000), (secs + 50_000, secs + 600)]
    {
        let h = codex_headers("100", "100", primary_reset, secondary_reset);
        let hold = codex_hold_seconds(&h, now).expect("a hold");
        assert!(
            (49_995..=50_000).contains(&hold),
            "both windows are spent, so the hold runs to the later reset, got {hold}"
        );
    }
}

/// A refusal: a 429 carrying `x-codex-rate-limit-reached-type` is the one shape
/// that marks the quota `rejected`, and the hold runs to the reset of the
/// window that is full.
#[test]
fn a_codex_429_with_a_reached_type_marks_the_quota_rejected_and_names_the_hold() {
    let now = OffsetDateTime::now_utc();
    let secs = now.unix_timestamp();
    let mut h = codex_headers("100", "12", secs + 1800, secs + 86400);
    h.push("x-codex-rate-limit-reached-type", "primary");
    let mut q = Quota::default();
    assert!(
        q.update_from_codex_refusal(
            &h,
            429,
            CodexQuotaRefusal::parse(&h, &[], OffsetDateTime::now_utc()).as_ref()
        ),
        "a 429's windows are folded exactly like a 200's"
    );
    assert_eq!(q.status.as_deref(), Some("rejected"));
    let hold = codex_hold_seconds(&h, now).expect("a hold");
    assert!(
        (1795..=1800).contains(&hold),
        "hold runs to the exhausted window's reset, got {hold}"
    );
}

/// The hold follows the window that is FULL, not the slot listed first: a
/// five-hour window at 30% does not free an account whose weekly is at 100%.
#[test]
fn codex_hold_prefers_the_window_that_is_actually_full() {
    let now = OffsetDateTime::now_utc();
    let secs = now.unix_timestamp();
    let h = codex_headers("30", "100", secs + 600, secs + 50_000);
    let hold = codex_hold_seconds(&h, now).unwrap();
    assert!(
        (49_995..=50_000).contains(&hold),
        "hold runs to the full weekly window's reset, got {hold}"
    );
}

#[test]
fn codex_refusal_does_not_wait_for_a_partly_used_window() {
    let now = OffsetDateTime::now_utc();
    let headers = codex_headers(
        "10",
        "35",
        now.unix_timestamp() + 600,
        now.unix_timestamp() + 6 * 86400,
    );
    assert_eq!(codex_hold_seconds(&headers, now), None);
}

#[test]
fn codex_refusal_discards_past_body_resets() {
    let now = OffsetDateTime::now_utc();
    let body = serde_json::json!({"error": {"type": "usage_limit_reached", "resets_at": now.unix_timestamp() - 5}});
    let headers = TestHeaders::new(&[]);
    assert_eq!(
        CodexQuotaRefusal::parse(&headers, body.to_string().as_bytes(), now)
            .expect("quota refusal with an elapsed reset")
            .reset,
        None
    );
    let relative = serde_json::json!({"error": {"type": "usage_limit_reached", "resets_at": now.unix_timestamp() - 5, "resets_in_seconds": 30}});
    assert_eq!(
        CodexQuotaRefusal::parse(&headers, relative.to_string().as_bytes(), now)
            .expect("quota refusal with a relative reset")
            .reset,
        Some(now + Duration::seconds(30))
    );
}

/// Each parser reads only its own provider's family, so a response folded
/// through the wrong one fabricates nothing: an Anthropic header is silence to
/// the Codex parser (no window, and so no `allowed` either), and a Codex block
/// is silence to the Anthropic one.
#[test]
fn anthropic_headers_do_not_feed_the_codex_parser_and_vice_versa() {
    let anthropic = TestHeaders::new(&[("anthropic-ratelimit-unified-5h-utilization", "0.5")]);
    let mut q = Quota::default();
    assert!(!q.update_from_codex_refusal(&anthropic, 200, None));
    assert!(q.five_hour.is_none());
    assert!(
        q.status.is_none(),
        "a 200 that carried no Codex window is no evidence about the account"
    );
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let mut q2 = Quota::default();
    assert!(!q2.update_from_headers(&codex_headers("25", "3", now + 1, now + 2)));
    assert!(q2.five_hour.is_none());
}

/// The shape the real backend sends, measured 2026-10-12 on two Pro logins, one
/// exhausted and one idle: ONLY the weekly window, in the PRIMARY slot,
/// and no secondary at all. Length mapping lands it in `seven_day` (by position
/// it would have filled `five_hour` with a weekly number and a weekly reset); no
/// five-hour window was read, so the method answers `false`; and the status still
/// follows the response, because a 200 is evidence whichever slot carried it.
#[test]
fn codex_weekly_only_in_the_primary_slot_fills_seven_day_and_leaves_five_hour_empty() {
    let now = OffsetDateTime::now_utc();
    let secs = now.unix_timestamp();
    let weekly_only = TestHeaders::new(&[
        ("x-codex-primary-used-percent", "100"),
        ("x-codex-primary-window-minutes", "10080"),
        ("x-codex-primary-reset-at", &(secs + 86400).to_string()),
    ]);
    let mut q = Quota::default();
    assert!(
        !q.update_from_codex_refusal(&weekly_only, 200, None),
        "no five-hour window was read"
    );
    assert!(
        q.five_hour.is_none(),
        "a weekly window never lands in five_hour"
    );
    let sd = q.seven_day.expect("the weekly window lands in seven_day");
    assert!(
        (sd.utilization - 1.0).abs() < 1e-9,
        "100 percent is 1.0, got {}",
        sd.utilization
    );
    assert_eq!(sd.reset.unwrap().unix_timestamp(), secs + 86400);
    assert_eq!(
        q.status.as_deref(),
        Some("allowed"),
        "a 200 reads as allowed whichever slot carried the window"
    );
    let hold = codex_hold_seconds(&weekly_only, now).expect("a hold");
    assert!(
        (86_395..=86_400).contains(&hold),
        "the hold names the weekly window's reset, got {hold}"
    );
}

/// Codex's numbers follow this module's rules for Anthropic's (Finding #11): a
/// non-finite `used-percent` is ABSENT. It creates no window, and it cannot
/// outrank a real 100% when the hold picks its window — NaN compares false, so a
/// NaN in the first slot would otherwise keep that slot and hold the account to
/// the wrong reset. And a reset no clock can reach is no hold, not an overflow.
#[test]
fn codex_non_finite_or_unreachable_numbers_are_absent_not_fatal() {
    let now = OffsetDateTime::now_utc();
    let secs = now.unix_timestamp();
    let nan_first = codex_headers("nan", "100", secs + 600, secs + 50_000);
    let mut q = Quota::default();
    assert!(
        !q.update_from_codex_refusal(&nan_first, 200, None),
        "a NaN five-hour window is not a read"
    );
    assert!(q.five_hour.is_none(), "and it creates no window");
    let hold = codex_hold_seconds(&nan_first, now).expect("a hold");
    assert!(
        (49_995..=50_000).contains(&hold),
        "the window that is really full still names the hold, got {hold}"
    );

    let unreachable = TestHeaders::new(&[
        ("x-codex-primary-used-percent", "100"),
        ("x-codex-primary-reset-at", &i64::MIN.to_string()),
    ]);
    assert_eq!(
        codex_hold_seconds(&unreachable, now),
        None,
        "a reset before any clock is ignored without overflow"
    );
}

// --- Codex: which windows a reading carried, and the facts beside them ----

/// The windows a Codex quota says its plan has, as the status wire names them.
fn carried(q: &Quota) -> Option<Vec<String>> {
    q.codex.windows().map(|w| w.wire_names())
}

fn names(list: &[&str]) -> Option<Vec<String>> {
    Some(list.iter().map(ToString::to_string).collect())
}

/// The shape a ChatGPT Pro login reports: its weekly window only, in the PRIMARY slot.
fn pro_codex_headers(reset: i64) -> TestHeaders {
    TestHeaders::new(&[
        ("x-codex-primary-used-percent", "40"),
        ("x-codex-primary-window-minutes", "10080"),
        ("x-codex-primary-reset-at", &reset.to_string()),
    ])
}

/// Which windows a plan has is what a reading carried: both on the two-window shape, the
/// weekly one alone on the Pro shape. A later response that carried no window at all is
/// silence, not evidence that the windows went away, and before any reading the set is
/// unknown rather than empty.
#[test]
fn codex_headers_record_which_windows_carried_a_value() {
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let mut two = Quota::default();
    assert!(two.update_from_codex_refusal(
        &codex_headers("25", "3", now + 3600, now + 86_400),
        200,
        None,
    ));
    assert_eq!(carried(&two), names(&["fiveHour", "sevenDay"]));

    let mut pro = Quota::default();
    assert!(!pro.update_from_codex_refusal(&pro_codex_headers(now + 86_400), 200, None));
    assert_eq!(carried(&pro), names(&["sevenDay"]));

    assert!(!pro.update_from_codex_refusal(
        &TestHeaders::new(&[("content-type", "text/event-stream")]),
        200,
        None,
    ));
    assert_eq!(
        carried(&pro),
        names(&["sevenDay"]),
        "a response that carried no window leaves the set alone"
    );

    assert_eq!(
        Quota::default().codex.windows(),
        None,
        "before any reading the windows are unknown, not none"
    );
}

/// A slot that carries no data is not a window, so it cannot add a five-hour window to a
/// plan that reports only its weekly one: a `0` with no length and no reset, or a value
/// that is not a number at all. Each placeholder is tried in the secondary slot beside the
/// Pro shape, and in the PRIMARY slot, which is the five-hour slot by position, beside a
/// weekly window the secondary slot carries by its length. The primary slot is the case only
/// the no-data rule decides: nothing else stops a placeholder there from filing a five-hour
/// window.
///
/// Watched red: count every slot that has a percent as a window, before the no-data rule,
/// and the primary-slot placeholder reads `["fiveHour", "sevenDay"]`.
#[test]
fn an_empty_five_hour_slot_is_not_a_window() {
    let now = OffsetDateTime::now_utc().unix_timestamp();
    for (label, placeholder) in [("0, no length, no reset", "0"), ("an empty value", "")] {
        let mut h = pro_codex_headers(now + 86_400);
        h.push("x-codex-secondary-used-percent", placeholder);
        let mut q = Quota::default();
        assert!(!q.update_from_codex_refusal(&h, 200, None), "{label}");
        assert_eq!(carried(&q), names(&["sevenDay"]), "{label}");
        assert!(q.five_hour.is_none(), "{label}: {:?}", q.five_hour);

        let mut h = TestHeaders::new(&[
            ("x-codex-secondary-used-percent", "40"),
            ("x-codex-secondary-window-minutes", "10080"),
            ("x-codex-secondary-reset-at", &(now + 86_400).to_string()),
        ]);
        h.push("x-codex-primary-used-percent", placeholder);
        let mut q = Quota::default();
        assert!(
            !q.update_from_codex_refusal(&h, 200, None),
            "{label}, in the primary slot"
        );
        assert_eq!(
            carried(&q),
            names(&["sevenDay"]),
            "{label}, in the primary slot"
        );
        assert!(
            q.five_hour.is_none(),
            "{label}, in the primary slot: {:?}",
            q.five_hour
        );
    }
}

/// Each window keeps the length it reported, in minutes, whichever slot it came in: 300
/// and 10 080 on the two-window shape, and a 30-day window (43 200) still fills the weekly
/// bucket. Two windows in one bucket keep the length of the one whose utilization the
/// bucket keeps (the fuller), and the longer length on equal utilization. A
/// `window-minutes` of 0 is no length: the slot is filed by its position, with none.
#[test]
fn codex_headers_keep_each_windows_length() {
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let read = |h: &TestHeaders| {
        let mut q = Quota::default();
        let read_five_hour = q.update_from_codex_refusal(h, 200, None);
        let windows = q.codex.windows().expect("the reading carried a window");
        assert_eq!(
            read_five_hour,
            windows.five_hour.is_some(),
            "the return value says whether the five-hour window was read"
        );
        windows
    };
    let window = |minutes: Option<u32>| Some(CodexWindow { minutes });

    let two = read(&codex_headers("25", "3", now + 3600, now + 86_400));
    assert_eq!(two.five_hour, window(Some(300)));
    assert_eq!(two.seven_day, window(Some(10_080)));

    let thirty_days = read(&TestHeaders::new(&[
        ("x-codex-secondary-used-percent", "20"),
        ("x-codex-secondary-window-minutes", "43200"),
        ("x-codex-secondary-reset-at", &(now + 86_400).to_string()),
    ]));
    assert_eq!(thirty_days.seven_day, window(Some(43_200)));
    assert_eq!(thirty_days.five_hour, None);

    for ((primary_used, secondary_used), kept) in [
        (("40", "90"), 43_200),
        (("90", "40"), 10_080),
        (("50", "50"), 43_200),
    ] {
        let weekly = read(&TestHeaders::new(&[
            ("x-codex-primary-used-percent", primary_used),
            ("x-codex-primary-window-minutes", "10080"),
            ("x-codex-primary-reset-at", &(now + 86_400).to_string()),
            ("x-codex-secondary-used-percent", secondary_used),
            ("x-codex-secondary-window-minutes", "43200"),
            ("x-codex-secondary-reset-at", &(now + 200_000).to_string()),
        ]));
        assert_eq!(
            weekly.seven_day,
            window(Some(kept)),
            "10080 at {primary_used}%, 43200 at {secondary_used}%"
        );
        assert_eq!(weekly.five_hour, None);
    }

    let no_length = read(&TestHeaders::new(&[
        ("x-codex-primary-used-percent", "30"),
        ("x-codex-primary-window-minutes", "0"),
        ("x-codex-primary-reset-at", &(now + 3600).to_string()),
    ]));
    assert_eq!(no_length.five_hour, window(None));
    assert_eq!(no_length.seven_day, None);
}

/// The refusal's kind and the account's credits are read off the response headers, which
/// the proxy strips from a pooled answer only after this fold. The kind lives exactly as long
/// as the refusal: set with `rejected`, cleared by a 2xx that carried a window, never brought
/// in by a 2xx, and cleared again when a probe sees the window roll. The credits are the last
/// reading that reported them: a response without them leaves them alone.
///
/// Watched red: drop the clear from `drop_rejection_if_a_window_rolled` and let the accessor
/// ignore `status`, and the last step still reads the refusal's kind after it rolled.
#[test]
fn codex_headers_read_the_reached_type_and_credits_before_the_strip() {
    use tcr_status_wire::CodexCredits;
    let now = OffsetDateTime::now_utc().unix_timestamp();

    let mut refusal = codex_headers("100", "12", now + 1800, now + 86_400);
    refusal.push(CODEX_REACHED_TYPE_HEADER, "rate_limit_reached");
    refusal.push("x-codex-credits-has-credits", "true");
    refusal.push("x-codex-credits-unlimited", "false");
    refusal.push("x-codex-credits-balance", "12.50");
    let mut q = Quota::default();
    assert!(q.update_from_codex_refusal(
        &refusal,
        429,
        CodexQuotaRefusal::parse(&refusal, &[], OffsetDateTime::now_utc()).as_ref(),
    ));
    assert_eq!(q.status.as_deref(), Some("rejected"));
    assert_eq!(q.codex_limit_reached_type(), Some("rate_limit_reached"));
    let credits = Some(CodexCredits {
        has_credits: true,
        unlimited: false,
        balance: Some("12.50".into()),
    });
    assert_eq!(q.codex.credits, credits);

    assert!(q.update_from_codex_refusal(
        &codex_headers("10", "5", now + 1800, now + 86_400),
        200,
        None,
    ));
    assert_eq!(q.status.as_deref(), Some("allowed"));
    assert_eq!(
        q.codex_limit_reached_type(),
        None,
        "a 2xx that carried a window ends the refusal, and its kind with it"
    );
    assert_eq!(
        q.codex.credits, credits,
        "a response without the credits headers leaves the last reading alone"
    );

    assert!(!q.update_from_codex_refusal(
        &TestHeaders::new(&[("x-codex-credits-has-credits", "FALSE")]),
        200,
        None,
    ));
    assert_eq!(
        q.codex.credits,
        Some(CodexCredits {
            has_credits: false,
            unlimited: false,
            balance: None,
        }),
        "the flag is read in any case, unlimited is false when absent, and no balance is none"
    );

    assert!(!q.update_from_codex_refusal(
        &TestHeaders::new(&[(CODEX_REACHED_TYPE_HEADER, "primary")]),
        200,
        None,
    ));
    assert_eq!(
        q.codex_limit_reached_type(),
        None,
        "a 200 is no refusal, whatever it carries"
    );

    let mut rolled = Quota {
        seven_day: Some(QuotaWindow {
            utilization: 1.0,
            reset: Some(OffsetDateTime::now_utc() + Duration::days(3)),
        }),
        status: Some("rejected".to_string()),
        ..Quota::default()
    };
    rolled.codex.limit_reached_type = Some("rate_limit_reached".to_string());
    rolled.apply_usage(&crate::records::probe::Usage {
        seven_day: Some(crate::records::probe::UsageBucket {
            utilization: Some(0.02),
            reset_at_ms: None,
        }),
        ..crate::records::probe::Usage::default()
    });
    assert_eq!(rolled.status, None, "the weekly window rolled");
    assert_eq!(
        rolled.codex.limit_reached_type, None,
        "the refusal's kind goes with the refusal"
    );
    assert_eq!(rolled.codex_limit_reached_type(), None);
}
