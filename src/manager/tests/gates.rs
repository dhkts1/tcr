//! Account gates, and the retry-after and exhaustion hints built from them.

use super::*;

/// When the only future reset across the fleet is the model-scoped weekly
/// (`seven_day_oi`) window — the bucket Fable requests gate on —
/// `retry_after_hint` advertises that reset, not the 60s fallthrough default.
#[test]
fn retry_after_hint_uses_seven_day_oi_window() {
    let manager = manager_of(vec![account("a", 0)]);
    let now = OffsetDateTime::now_utc();
    // Only the 7d_oi window has a future reset (~2h out); five_hour / seven_day /
    // rate-limit hold are all absent, so before the fix the hint fell through to 60s.
    let reset_ms = odt_to_ms(now) + 2 * 3_600_000;
    manager.apply_usage(
        0,
        &Usage {
            five_hour: crate::records::probe::WindowReading::Unread,
            seven_day: None,
            seven_day_oi: Some(UsageBucket {
                utilization: Some(0.99),
                reset_at_ms: Some(reset_ms),
            }),
            extra_usage_usd: None,
            codex: None,
            claude_resets: None,
        },
    );
    // A FABLE request (`is_fable = true`) is the only one gated by the 7d_oi
    // bucket, so it is what makes that window drive the hint. A non-Fable
    // request ignores 7d_oi entirely and falls through to the 60s default.
    let hint = manager.retry_after_hint(now, true, crate::base::provider::Provider::Anthropic);
    assert!(
        hint > 60,
        "the 7d_oi reset must drive the hint past the 60s default, got {hint}"
    );
    let expected = ((reset_ms - odt_to_ms(now) + 999) / 1000).max(1);
    assert_eq!(
        hint, expected,
        "hint equals the 7d_oi reset delta in seconds"
    );
    // The general (non-Fable) view does NOT see the Fable-only weekly, so with
    // no other gate the hint falls back to the 60s default — proof the
    // `is_fable` scoping is honoured, not hard-coded.
    assert_eq!(
        manager.retry_after_hint(now, false, crate::base::provider::Provider::Anthropic),
        60,
        "a non-Fable request ignores the 7d_oi bucket"
    );
}

/// A quota window at `util`, resetting at `reset` (`None` = unknown reset).
fn window(util: f64, reset: Option<OffsetDateTime>) -> crate::records::quota::QuotaWindow {
    crate::records::quota::QuotaWindow {
        utilization: util,
        reset,
    }
}

/// A fresh active runtime with empty quota, for the [`Manager::account_gate`] tests.
fn gate_runtime() -> AccountRuntime {
    AccountRuntime::from_config(&account("gate", 0), false)
}

/// One fixture for [`gate_and_hard_ok_agree_on_every_variant`]: a runtime
/// that exhibits one [`GateReason`], whether that block is ACCOUNT-level
/// (`account_hard_ok == false`) or request-scoped, and the two group sets
/// the request is judged against.
struct GateCase {
    label: &'static str,
    runtime: AccountRuntime,
    account_level: bool,
    reserved: HashSet<String>,
    parked: HashSet<String>,
}

impl GateCase {
    /// The common shape: the runtime alone exhibits the gate, with nothing
    /// reserved and nothing parked. Only `Reserved` and `Parked` — the two
    /// gates that depend on more than the runtime — build one by hand.
    fn plain(label: &'static str, runtime: AccountRuntime, account_level: bool) -> Self {
        Self {
            label,
            runtime,
            account_level,
            reserved: HashSet::new(),
            parked: HashSet::new(),
        }
    }
}

#[test]
fn account_gate_ok_when_healthy() {
    let now = OffsetDateTime::now_utc();
    let a = gate_runtime();
    assert_eq!(
        Manager::account_gate(
            &a,
            0.90,
            None,
            now,
            odt_to_ms(now),
            false,
            None,
            &HashSet::new(),
            &HashSet::new()
        ),
        (GateReason::Ok, None)
    );
}

#[test]
fn account_gate_disabled_and_error_never_self_free() {
    let now = OffsetDateTime::now_utc();
    // Even with a gating window present, the terminal states dominate and carry
    // NO clear-instant — they never self-free.
    let mut disabled = gate_runtime();
    disabled.disabled = true;
    disabled.quota.five_hour = Some(window(0.99, Some(now + Duration::seconds(300))));
    assert_eq!(
        Manager::account_gate(
            &disabled,
            0.90,
            None,
            now,
            odt_to_ms(now),
            false,
            None,
            &HashSet::new(),
            &HashSet::new()
        ),
        (GateReason::Disabled, None)
    );

    let mut errored = gate_runtime();
    errored.status = AccountStatus::Error;
    errored.quota.five_hour = Some(window(0.99, Some(now + Duration::seconds(300))));
    assert_eq!(
        Manager::account_gate(
            &errored,
            0.90,
            None,
            now,
            odt_to_ms(now),
            false,
            None,
            &HashSet::new(),
            &HashSet::new()
        ),
        (GateReason::Login, None)
    );
}

#[test]
fn account_gate_binds_on_latest_clearing_window() {
    // 5h clears SOON, 7d clears LATER — the account frees only when BOTH clear,
    // so the reason is the later 7d gate and free_at is its (max) reset. A naive
    // "soonest reset" would wrongly report the 5h instant.
    let now = OffsetDateTime::now_utc();
    let soon = now + Duration::seconds(300);
    let later = now + Duration::seconds(5_000);
    let mut a = gate_runtime();
    a.quota.five_hour = Some(window(0.99, Some(soon)));
    a.quota.seven_day = Some(window(0.99, Some(later)));
    assert_eq!(
        Manager::account_gate(
            &a,
            0.90,
            None,
            now,
            odt_to_ms(now),
            false,
            None,
            &HashSet::new(),
            &HashSet::new()
        ),
        (GateReason::SevenDay, Some(later))
    );
}

#[test]
fn account_gate_unknown_reset_sorts_latest_and_hides_time() {
    // A window over threshold whose reset is unknown (None) is the longest
    // possible constraint: it becomes the reason and free_at is None (no promise
    // of a time), even beside a window with a known soon reset.
    let now = OffsetDateTime::now_utc();
    let mut a = gate_runtime();
    a.quota.five_hour = Some(window(0.99, Some(now + Duration::seconds(300))));
    a.quota.seven_day = Some(window(0.99, None));
    assert_eq!(
        Manager::account_gate(
            &a,
            0.90,
            None,
            now,
            odt_to_ms(now),
            false,
            None,
            &HashSet::new(),
            &HashSet::new()
        ),
        (GateReason::SevenDay, None)
    );
}

#[test]
fn account_gate_fable_weekly_only_gates_fable() {
    // The 7d_oi bucket gates a Fable evaluation and is invisible to the general
    // one — the exact `is_fable` split `eligible` makes.
    let now = OffsetDateTime::now_utc();
    let reset = now + Duration::seconds(4_000);
    let mut a = gate_runtime();
    a.quota.seven_day_oi = Some(window(0.99, Some(reset)));
    assert_eq!(
        Manager::account_gate(
            &a,
            0.90,
            None,
            now,
            odt_to_ms(now),
            false,
            None,
            &HashSet::new(),
            &HashSet::new()
        ),
        (GateReason::Ok, None),
        "the non-Fable view ignores the model-scoped weekly"
    );
    assert_eq!(
        Manager::account_gate(
            &a,
            0.90,
            None,
            now,
            odt_to_ms(now),
            true,
            None,
            &HashSet::new(),
            &HashSet::new()
        ),
        (GateReason::FableWeekly, Some(reset)),
        "a Fable evaluation gates on it"
    );
}

/// A near standard (API-key) token limit with a future `standard_reset` and no
/// other gate is reported as `(Standard, Some(reset))` — `account_gate` now sees
/// the same dimension `eligible`/`is_near` enforces.
#[test]
fn account_gate_gates_a_near_standard_limit() {
    let now = OffsetDateTime::now_utc();
    let reset = now + Duration::seconds(600);
    let mut a = gate_runtime();
    a.quota.tokens_limit = Some(1_000);
    a.quota.tokens_remaining = Some(50); // 95% spent, over the 0.90 threshold
    a.quota.standard_reset = Some(reset);
    assert_eq!(
        Manager::account_gate(
            &a,
            0.90,
            None,
            now,
            odt_to_ms(now),
            false,
            None,
            &HashSet::new(),
            &HashSet::new()
        ),
        (GateReason::Standard, Some(reset))
    );
}

/// The exact G2 scenario: an account with BOTH a short transient Hold (+8s) AND
/// a near standard limit (reset +600s) must bind on the later-clearing Standard
/// gate, so `retry_after_hint` reports the true 600s recovery — not the 8s Hold
/// that would trigger a soft-wait then hard-fail anyway.
#[test]
fn account_gate_standard_reset_outlasts_a_short_hold() {
    let now = OffsetDateTime::now_utc();
    let now_ms = odt_to_ms(now);
    let reset = now + Duration::seconds(600);
    let mut a = AccountRuntime::from_config(&account("std-hold", 0), false);
    a.switch_threshold = Some(0.90);
    a.rate_limited_until_ms = Some(now_ms + 8_000); // short Hold, +8s
    a.quota.requests_limit = Some(200);
    a.quota.requests_remaining = Some(5); // 97.5% spent
    a.quota.standard_reset = Some(reset);

    // The Standard gate (later reset) wins max_by_key over the +8s Hold.
    assert_eq!(
        Manager::account_gate(
            &a,
            0.90,
            None,
            now,
            now_ms,
            false,
            None,
            &HashSet::new(),
            &HashSet::new()
        ),
        (GateReason::Standard, Some(reset)),
        "the standard reset outlasts the short hold"
    );

    // retry_after_hint must now reflect the real 600s, not the 8s Hold.
    let manager = Manager::from_runtimes(vec![a]);
    let hint = manager.retry_after_hint(now, false, crate::base::provider::Provider::Anthropic);
    let expected = ((odt_to_ms(reset) - now_ms + 999) / 1000).max(1);
    assert_eq!(hint, expected, "hint must be the 600s reset, got {hint}");
    assert!(hint > 500, "must not report the 8s hold, got {hint}");
}

/// G-a: an OAuth account (all standard fields `None`) produces NO Standard gate,
/// so `account_gate` is byte-identical to its pre-fix behavior for OAuth.
#[test]
fn account_gate_ignores_standard_for_oauth() {
    let now = OffsetDateTime::now_utc();
    let a = gate_runtime(); // all standard fields None by default
    assert_eq!(
        Manager::account_gate(
            &a,
            0.90,
            None,
            now,
            odt_to_ms(now),
            false,
            None,
            &HashSet::new(),
            &HashSet::new()
        ),
        (GateReason::Ok, None),
        "OAuth accounts never gate on the standard dimension"
    );
}

/// An expired `standard_reset` (in the PAST) means the upstream limit has
/// refreshed, so a spent count must NOT keep gating — mirrors `is_near`'s expiry
/// rule so a spent standard window never pins an API-key account out forever.
#[test]
fn account_gate_ignores_expired_standard() {
    let now = OffsetDateTime::now_utc();
    let mut a = gate_runtime();
    a.quota.tokens_limit = Some(1_000);
    a.quota.tokens_remaining = Some(10); // 99% spent, over threshold
    a.quota.standard_reset = Some(now - Duration::seconds(1)); // already refreshed
    assert_eq!(
        Manager::account_gate(
            &a,
            0.90,
            None,
            now,
            odt_to_ms(now),
            false,
            None,
            &HashSet::new(),
            &HashSet::new()
        ),
        (GateReason::Ok, None),
        "an expired standard window no longer gates"
    );
}

#[test]
fn rejected_account_reports_a_rejected_gate() {
    let now = OffsetDateTime::now_utc();
    let now_ms = odt_to_ms(now);
    let mut a = gate_runtime();
    a.quota.status = Some("rejected".to_string());

    // Was `Ok`: `account_hard_ok` held the account out on `rejected` while
    // `account_gate` had no arm for it, so the TUI showed a rejected account as
    // healthy and in rotation.
    assert_eq!(
        Manager::account_gate(
            &a,
            0.90,
            None,
            now,
            now_ms,
            false,
            None,
            &HashSet::new(),
            &HashSet::new()
        ),
        (GateReason::Rejected, None)
    );
    assert!(
        !Manager::account_hard_ok(
            &a,
            now_ms,
            None,
            &HashSet::new(),
            &HashSet::new(),
            WarmWindow::DEFAULT
        ),
        "and it stays hard-gated, exactly as before"
    );

    // Terminal, so it dominates a live window and carries NO clear-instant —
    // `retry_after_hint` reads `free_at`, and a rejected account was never going
    // to come back at its 5h reset.
    a.quota.five_hour = Some(window(0.99, Some(now + Duration::seconds(300))));
    assert_eq!(
        Manager::account_gate(
            &a,
            0.90,
            None,
            now,
            now_ms,
            false,
            None,
            &HashSet::new(),
            &HashSet::new()
        ),
        (GateReason::Rejected, None),
        "a rejected account must not advertise a window reset as its recovery"
    );
}

/// A refused account as the snapshot sees it: `provider`, `rejected`, and the two windows
/// as (utilization, reset from `now`). Thresholds are the default 0.95.
fn refused_runtime(
    provider: crate::base::provider::Provider,
    now: OffsetDateTime,
    five_hour: Option<(f64, Duration)>,
    seven_day: Option<(f64, Duration)>,
) -> AccountRuntime {
    let window = |(utilization, reset_in): (f64, Duration)| crate::records::quota::QuotaWindow {
        utilization,
        reset: Some(now + reset_in),
    };
    let mut account = AccountRuntime::from_config(&account("refused", 0), false);
    account.provider = provider;
    account.quota.status = Some("rejected".into());
    account.quota.five_hour = five_hour.map(window);
    account.quota.seven_day = seven_day.map(window);
    account
}

/// A manager over one refused Codex account, as [`refused_runtime`] builds it.
fn refused_codex_manager(
    now: OffsetDateTime,
    five_hour: Option<(f64, Duration)>,
    seven_day: Option<(f64, Duration)>,
) -> Arc<Manager> {
    Manager::from_runtimes(vec![refused_runtime(
        crate::base::provider::Provider::Codex,
        now,
        five_hour,
        seven_day,
    )])
}

/// A manager over one refused Codex account whose windows are both half spent, so no
/// window explains the refusal.
fn codex_refused_with_neither_window_full(now: OffsetDateTime) -> Arc<Manager> {
    refused_codex_manager(
        now,
        Some((0.5, Duration::hours(2))),
        Some((0.5, Duration::days(3))),
    )
}

/// The (gate, free-at in Unix seconds) the snapshot reports for account 0.
fn reported_gate(manager: &Manager, now: OffsetDateTime) -> (GateReason, Option<i64>) {
    let row = &manager.snapshot(now).accounts[0];
    (row.gate, row.free_at.map(OffsetDateTime::unix_timestamp))
}

/// **A Codex account at its weekly limit reports `SevenDay` and the weekly reset**, not a
/// `Rejected` with no time: ChatGPT's usage-limit refusal names the window, and the window
/// outranks the hour-long hold the refusal also armed.
///
/// Watched red: return the input unchanged from `codex_refusal_gate` and this reads
/// `Rejected`.
#[test]
fn a_weekly_refusal_reports_seven_day_and_its_reset() {
    let now = OffsetDateTime::now_utc();
    let manager = refused_codex_manager(
        now,
        Some((0.3, Duration::hours(2))),
        Some((1.0, Duration::days(3))),
    );
    manager.mark_rate_limited(0, 3600);
    assert_eq!(
        reported_gate(&manager, now),
        (
            GateReason::SevenDay,
            Some((now + Duration::days(3)).unix_timestamp())
        )
    );
}

/// The five-hour window that refused an account is its gate, with that window's reset. With
/// both windows full, both have to roll before it serves, so the later reset, the weekly
/// one, is the gate.
///
/// Watched red: take the earliest reset instead of the latest and the both-full case reads
/// `FiveHour`.
#[test]
fn a_five_hour_refusal_reports_five_hour() {
    let now = OffsetDateTime::now_utc();
    let manager = refused_codex_manager(
        now,
        Some((1.0, Duration::minutes(52))),
        Some((0.17, Duration::days(5))),
    );
    assert_eq!(
        reported_gate(&manager, now),
        (
            GateReason::FiveHour,
            Some((now + Duration::minutes(52)).unix_timestamp())
        )
    );

    let both_full = refused_codex_manager(
        now,
        Some((1.0, Duration::hours(1))),
        Some((1.0, Duration::days(3))),
    );
    assert_eq!(
        reported_gate(&both_full, now),
        (
            GateReason::SevenDay,
            Some((now + Duration::days(3)).unix_timestamp())
        ),
        "both full: the later reset is when the account serves again"
    );
}

/// A refusal no full window explains (credits, a spend cap) but that armed a hold reports
/// the hold, until the hold's end.
#[test]
fn a_refusal_with_no_full_window_holds_until_its_reset() {
    let now = OffsetDateTime::now_utc();
    let manager = codex_refused_with_neither_window_full(now);
    manager.mark_rate_limited(0, 1800);
    let row = manager.snapshot(now).accounts[0].clone();
    assert!(row.rate_limited_until.is_some(), "the hold is armed");
    assert_eq!(row.gate, GateReason::Hold);
    assert_eq!(row.free_at, row.rate_limited_until);
}

/// **A standing refusal whose live hold an upstream 403 armed reports `Refused`**, the gate
/// a 403's hold reads on every other row, until the hold's end. A refused Codex account is
/// back in the pool once the refusal's own hold lapses, since the pick reads the hold and
/// the windows and never `rejected`, so the live hold can be a 403's
/// (`Manager::mark_refused`). The remap reports a live hold under the name `account_gate`
/// gives it.
///
/// Watched red: report every live hold as `Hold`, and this reads `Hold`.
#[test]
fn a_refusal_held_by_an_upstream_403_reports_refused() {
    let now = OffsetDateTime::now_utc();
    let manager = codex_refused_with_neither_window_full(now);
    manager.mark_refused(0);
    let row = manager.snapshot(now).accounts[0].clone();
    assert!(row.rate_limited_until.is_some(), "the 403's hold is armed");
    assert_eq!(row.gate, GateReason::Refused);
    assert_eq!(row.free_at, row.rate_limited_until);
}

/// **The window that refused an account keeps its gate past its reset, with no time, until a
/// probe confirms the roll.** The clock says the window has rolled; a usage probe, or a
/// served 2xx that carries a window, ends the refusal. Reading `Rejected` in between would
/// turn a spent row into a refused one for the minutes until the next probe. The hold the
/// refusal armed, to that same reset, has lapsed by then too. Weekly and five-hour alike.
///
/// Watched red: without the rolled-window rule, past the reset this reads
/// `(Rejected, None)`.
#[test]
fn a_refusing_window_past_its_reset_keeps_its_gate_until_a_probe_confirms_the_roll() {
    let now = OffsetDateTime::now_utc();
    let weekly = refused_codex_manager(now, None, Some((1.0, Duration::minutes(1))));
    weekly.mark_rate_limited(0, 60);
    assert_eq!(
        reported_gate(&weekly, now),
        (
            GateReason::SevenDay,
            Some((now + Duration::minutes(1)).unix_timestamp())
        ),
        "before the reset: the window, and its reset"
    );
    assert_eq!(
        reported_gate(&weekly, now + Duration::minutes(2)),
        (GateReason::SevenDay, None),
        "past the reset, no probe yet: the same window, with no time"
    );

    let five_hour = refused_codex_manager(
        now,
        Some((1.0, Duration::minutes(1))),
        Some((0.3, Duration::days(5))),
    );
    assert_eq!(
        reported_gate(&five_hour, now + Duration::minutes(2)),
        (GateReason::FiveHour, None)
    );
}

/// With neither a full window nor a hold, nothing names a time: still `Rejected`.
#[test]
fn a_refusal_with_neither_stays_rejected() {
    let now = OffsetDateTime::now_utc();
    let manager = codex_refused_with_neither_window_full(now);
    assert_eq!(reported_gate(&manager, now), (GateReason::Rejected, None));
}

/// Anthropic's `rejected` is its own verdict and names no time, so its row is exactly what
/// `account_gate` says, a full weekly window or not.
#[test]
fn an_anthropic_rejected_row_is_unchanged() {
    let now = OffsetDateTime::now_utc();
    let manager = Manager::from_runtimes(vec![refused_runtime(
        crate::base::provider::Provider::Anthropic,
        now,
        None,
        Some((1.0, Duration::days(3))),
    )]);
    assert_eq!(reported_gate(&manager, now), (GateReason::Rejected, None));
}

/// The drift guard, and the reason R7 exists. [`Manager::account_gate`] (what
/// the TUI and `retry_after_hint` read) and [`Manager::account_hard_ok`] (whether
/// a session may lose its pin) once kept two hand-maintained gate lists, and they
/// drifted: `rejected` was on one and missing from the other.
///
/// The two are deliberately NOT equal — a quota window or a short hold gates the
/// display while leaving the pin alone — so this pins the RELATIONSHIP per
/// variant instead. The classification below is exhaustive over [`GateReason`],
/// so adding a variant stops compiling until someone decides which side of the
/// account/request line it falls on.
#[test]
fn gate_and_hard_ok_agree_on_every_variant() {
    let now = OffsetDateTime::now_utc();
    let now_ms = odt_to_ms(now);
    let reset = now + Duration::seconds(5_000);

    const ALL: [GateReason; 12] = [
        GateReason::Ok,
        GateReason::Hold,
        GateReason::Refused,
        GateReason::FiveHour,
        GateReason::SevenDay,
        GateReason::FableWeekly,
        GateReason::Standard,
        GateReason::Login,
        GateReason::Disabled,
        GateReason::Rejected,
        GateReason::Reserved,
        GateReason::Parked,
    ];

    for reason in ALL {
        // Only a Fable-scoped evaluation can ever surface the model-scoped gate.
        let is_fable = reason == GateReason::FableWeekly;

        // Per case: a label, a runtime that actually exhibits `reason`, whether
        // that block is ACCOUNT-level (`account_hard_ok == false`) or
        // request-scoped (`account_hard_ok` stays true, the pin survives), and
        // and the reserved / parked group sets the request evaluates
        // against (both empty for every reason but `Reserved` and `Parked`
        // themselves, the two whose gate depends on more than the runtime
        // alone).
        let cases: Vec<GateCase> = match reason {
            GateReason::Ok => vec![GateCase::plain("healthy", gate_runtime(), true)],

            // Terminal: a fact about the credential, for every model class.
            GateReason::Disabled => {
                let mut a = gate_runtime();
                a.disabled = true;
                vec![GateCase::plain("operator-disabled", a, false)]
            }
            GateReason::Login => {
                let mut a = gate_runtime();
                a.status = AccountStatus::Error;
                vec![GateCase::plain("dead credential", a, false)]
            }
            GateReason::Rejected => {
                let mut a = gate_runtime();
                a.quota.status = Some("rejected".to_string());
                vec![GateCase::plain("upstream rejected", a, false)]
            }
            // Reservation is not a preference: unrequested traffic (`group:
            // None`, matching every other case in this loop) against an
            // account in a reserved group is ACCOUNT-level-blocked, exactly
            // like the terminal gates above — see `Self::reserved_blocks`.
            GateReason::Reserved => {
                let mut a = gate_runtime();
                a.groups = vec!["codereview".to_string()];
                vec![GateCase {
                    label: "reserved group, unrequested traffic",
                    runtime: a,
                    account_level: false,
                    reserved: ["codereview".to_string()].into(),
                    parked: HashSet::new(),
                }]
            }
            // Parking is not a preference either, and unlike reservation it
            // does not even depend on the request's ask: a member of a
            // parked group is ACCOUNT-level-blocked for every request —
            // see `Self::parked_blocks`.
            GateReason::Parked => {
                let mut a = gate_runtime();
                a.groups = vec!["codereview".to_string()];
                vec![GateCase {
                    label: "parked group",
                    runtime: a,
                    account_level: false,
                    reserved: HashSet::new(),
                    parked: ["codereview".to_string()].into(),
                }]
            }

            // The one reason that splits on DURATION: past the cache TTL a hold
            // is account death, under it a timer worth keeping the pin for.
            GateReason::Hold => {
                let mut long = gate_runtime();
                long.rate_limited_until_ms = Some(now_ms + (CACHE_WARM_HOLD_SECS + 60) * 1_000);
                let mut short = gate_runtime();
                short.rate_limited_until_ms = Some(now_ms + 30_000);
                vec![
                    GateCase::plain("hold outliving the cache", long, false),
                    GateCase::plain("hold clearing while warm", short, true),
                ]
            }
            // A 403 refusal hold rides the same deadline as a rate-limit hold,
            // so it splits on duration exactly the same way; only its name
            // differs.
            GateReason::Refused => {
                let mut long = gate_runtime();
                long.rate_limited_until_ms = Some(now_ms + (CACHE_WARM_HOLD_SECS + 60) * 1_000);
                long.hold_cause = HoldCause::Refused;
                let mut short = gate_runtime();
                short.rate_limited_until_ms = Some(now_ms + 30_000);
                short.hold_cause = HoldCause::Refused;
                vec![
                    GateCase::plain("refusal hold outliving the cache", long, false),
                    GateCase::plain("refusal hold clearing while warm", short, true),
                ]
            }

            // Windows are per-request facts: they gate the display and every
            // serve decision, but must never move a session's pin.
            GateReason::FiveHour => {
                let mut a = gate_runtime();
                a.quota.five_hour = Some(window(0.99, Some(reset)));
                vec![GateCase::plain("5h over threshold", a, true)]
            }
            GateReason::SevenDay => {
                let mut a = gate_runtime();
                a.quota.seven_day = Some(window(0.99, Some(reset)));
                vec![GateCase::plain("7d over threshold", a, true)]
            }
            GateReason::FableWeekly => {
                let mut a = gate_runtime();
                a.quota.seven_day_oi = Some(window(0.99, Some(reset)));
                vec![GateCase::plain("7d_oi over threshold", a, true)]
            }
            GateReason::Standard => {
                let mut a = gate_runtime();
                a.quota.tokens_limit = Some(1_000);
                a.quota.tokens_remaining = Some(10); // 99% spent
                a.quota.standard_reset = Some(reset);
                vec![GateCase::plain("standard limit spent", a, true)]
            }
        };

        for case in cases {
            let GateCase {
                label,
                runtime,
                account_level,
                reserved,
                parked,
            } = case;
            let (gate, _) = Manager::account_gate(
                &runtime, 0.90, None, now, now_ms, is_fable, None, &reserved, &parked,
            );
            assert_eq!(gate, reason, "fixture `{label}` must exhibit {reason:?}");

            let hard_ok = Manager::account_hard_ok(
                &runtime,
                now_ms,
                None,
                &reserved,
                &parked,
                WarmWindow::DEFAULT,
            );
            assert_eq!(
                hard_ok, account_level,
                "`{label}`: account_hard_ok disagrees with {reason:?}'s classification"
            );

            // The invariant that actually broke: whatever `account_hard_ok` holds
            // out MUST have a reason to show for it. `rejected` violated this.
            if !hard_ok {
                assert_ne!(
                    gate,
                    GateReason::Ok,
                    "`{label}` is hard-gated but renders Ok — the gate lists have drifted"
                );
            }
        }
    }
}

/// Three accounts out of rotation at `now`, each with a 5-hour reset that misleads
/// differently:
///
/// - A, weekly-gated: its 5h resets SOON (200s) but its 7d stays over threshold until
///   5000s, so A does NOT actually return until 5000s.
/// - B, a dead credential (Error) that holds the SOONEST raw reset (100s). It never
///   self-frees, so it must contribute nothing to the hint.
/// - C, the TRUE first recovery: 5h-gated with a later reset (900s) and a healthy 7d,
///   so it genuinely returns at 900s.
fn weekly_gated_dead_and_five_hour_gated(now: OffsetDateTime) -> Arc<Manager> {
    let at = |secs: i64| now + Duration::seconds(secs);
    let mut a = AccountRuntime::from_config(&account("a", 0), false);
    a.switch_threshold = Some(0.90);
    a.quota.five_hour = Some(window(0.99, Some(at(200))));
    a.quota.seven_day = Some(window(0.99, Some(at(5_000))));
    let mut b = AccountRuntime::from_config(&account("b", 0), false);
    b.switch_threshold = Some(0.90);
    b.status = AccountStatus::Error;
    b.quota.five_hour = Some(window(0.99, Some(at(100))));
    let mut c = AccountRuntime::from_config(&account("c", 0), false);
    c.switch_threshold = Some(0.90);
    c.quota.five_hour = Some(window(0.99, Some(at(900))));
    Manager::from_runtimes(vec![a, b, c])
}

/// The live-incident shape: a fleet-exhausted 429's `retry-after` must name the
/// TRUE soonest recovery, never an earlier reset belonging to an account that
/// stays gated for another reason or never self-frees at all.
#[test]
fn retry_after_hint_skips_error_and_still_gated_accounts() {
    let now = OffsetDateTime::now_utc();
    let at = |secs: i64| now + Duration::seconds(secs);
    let manager = weekly_gated_dead_and_five_hour_gated(now);
    let hint = manager.retry_after_hint(now, false, crate::base::provider::Provider::Anthropic);

    // Correct = C's 900s reset. The pre-fix raw-min over every window's reset
    // returned ~100s: it counted B's reset (an Error account that never returns)
    // and A's 200s 5h reset (A stays gated on its weekly until 5000s). Both are
    // now skipped, so the min of the real free_at instants is C's 900s.
    let expected = ((odt_to_ms(at(900)) - odt_to_ms(now) + 999) / 1000).max(1);
    assert_eq!(hint, expected, "hint must be C's 900s reset, got {hint}");
    assert!(
        hint > 800,
        "must not report B's 100s or A's 200s reset, got {hint}"
    );
}

/// The full hint: the gate that TIMES the wait, and a count of every account
/// out of rotation by its gate. Same fleet as the test above, so the binding
/// gate is C's 5-hour window, while A's weekly and B's dead credential are
/// counted without being allowed to drive the time.
#[test]
fn exhaustion_hint_names_the_binding_gate_and_counts_every_gated_account() {
    let now = OffsetDateTime::now_utc();
    let at = |secs: i64| now + Duration::seconds(secs);
    let manager = weekly_gated_dead_and_five_hour_gated(now);
    let hint =
        manager.exhaustion_hint(now, false, crate::base::provider::Provider::Anthropic, None);
    assert_eq!(
        hint.binding,
        Some(GateReason::FiveHour),
        "C's 5h window times the wait"
    );
    assert_eq!(
        hint.free_at.map(odt_to_ms),
        Some(odt_to_ms(at(900))),
        "free_at is C's reset instant"
    );
    assert_eq!(
        hint.retry_after,
        manager.retry_after_hint(now, false, crate::base::provider::Provider::Anthropic),
        "the seconds half is byte-identical to retry_after_hint"
    );
    let counts: Vec<(GateReason, usize)> = hint.gated.iter().map(|(r, n)| (*r, *n)).collect();
    assert_eq!(
        counts,
        vec![
            (GateReason::FiveHour, 1),
            (GateReason::SevenDay, 1),
            (GateReason::Login, 1)
        ],
        "every gated account is counted under the gate that binds IT"
    );
}

/// When no account advertises a reset, the number stays the 60 sentinel the
/// proxy's soft-wait relies on, but `free_at`/`binding` are `None` so a
/// message can say "no reset known" instead of printing 60 as a measurement.
#[test]
fn exhaustion_hint_reports_an_unknown_reset_as_unknown_not_as_sixty_seconds() {
    let now = OffsetDateTime::now_utc();
    let mut a = AccountRuntime::from_config(&account("a", 0), false);
    a.switch_threshold = Some(0.90);
    a.quota.seven_day = Some(window(0.99, None));
    let manager = Manager::from_runtimes(vec![a]);
    let hint =
        manager.exhaustion_hint(now, false, crate::base::provider::Provider::Anthropic, None);
    assert_eq!(hint.retry_after, 60);
    assert_eq!(hint.free_at, None);
    assert_eq!(hint.binding, None);
    assert_eq!(hint.gated.get(&GateReason::SevenDay), Some(&1));
}

/// A request that only account 1 may serve (bound history, see `crate::stores::bound_tokens`) is
/// told when account 1 frees — 7 seconds — not when the SOONER account 0 does. The
/// fleet-wide hint over the same fleet is the control: it answers 2.
#[test]
fn a_single_account_hint_times_the_wait_on_that_account_alone() {
    let manager = Manager::from_runtimes(vec![
        AccountRuntime::from_config(&account("a", 0), false),
        AccountRuntime::from_config(&account("b", 0), false),
    ]);
    manager.mark_rate_limited(0, 2);
    manager.mark_rate_limited(1, 7);
    // Read AFTER the marks, so each hold has at most its own length left and rounds up to
    // exactly that many whole seconds.
    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager
            .exhaustion_hint(now, false, crate::base::provider::Provider::Anthropic, None)
            .retry_after,
        2,
        "the control: fleet-wide, the sooner account wins"
    );
    let hint = manager.exhaustion_hint_for_account(now, false, 1);
    assert_eq!(
        hint.retry_after, 7,
        "bound to account 1, the wait is ITS hold: {hint:?}"
    );
    assert_eq!(hint.binding, Some(GateReason::Hold));
    assert_eq!(
        hint.gated.get(&GateReason::Hold),
        Some(&1),
        "only the one account is counted: {hint:?}"
    );
}

/// **A 429's wait is timed on the request's own provider.** An account of the
/// other provider can never serve the request, so its reset says nothing about when this
/// one can be served: a Claude fleet out for seven seconds is not "back in two" because
/// a Codex account is, nor the reverse. Both orders, every entry point (the fleet hint,
/// its seconds half, and the group form a strict request uses), and the gate count names
/// only the provider's own account.
///
/// Watched red: drop the provider filter in `exhaustion_hint_over` and the sooner account
/// wins for both providers.
#[test]
fn codex_exhaustion_hint_folds_only_the_requests_own_provider() {
    use crate::base::provider::Provider;
    for (anthropic_hold, codex_hold) in [(7, 2), (2, 7)] {
        let mut codex = AccountRuntime::from_config(&account_in_groups("b", 0, &["g"]), false);
        codex.provider = Provider::Codex;
        let manager = Manager::from_runtimes(vec![
            AccountRuntime::from_config(&account_in_groups("a", 0, &["g"]), false),
            codex,
        ]);
        manager.mark_rate_limited(0, anthropic_hold);
        manager.mark_rate_limited(1, codex_hold);
        // Read AFTER the marks, so each hold rounds up to exactly its own length.
        let now = OffsetDateTime::now_utc();
        for (provider, hold) in [
            (Provider::Anthropic, anthropic_hold),
            (Provider::Codex, codex_hold),
        ] {
            let hint = manager.exhaustion_hint(now, false, provider, None);
            assert_eq!(
                hint.retry_after,
                hold,
                "{} is timed on its own account: {hint:?}",
                provider.as_str()
            );
            assert_eq!(
                hint.gated.get(&GateReason::Hold),
                Some(&1),
                "only {}'s own account is counted: {hint:?}",
                provider.as_str()
            );
            assert_eq!(manager.retry_after_hint(now, false, provider), hold);
            assert_eq!(
                manager.retry_after_hint_for_group(now, false, provider, "g"),
                hold,
                "the group form folds the provider too"
            );
        }
    }
}

/// **A Codex 429's wait is the earliest Codex window reset**, not the 60-second
/// placeholder: once `x-codex-*` headers fill a Codex account's weekly window, the
/// provider-scoped hint folds it like any other window over threshold. Driven through
/// `update_quota` with the shape the real backend sends (only the weekly window, in the
/// primary slot), so it is the dispatch that fills the window: an Anthropic account
/// handed the SAME headers learns nothing from them and times nothing.
///
/// Watched red, one arm of the dispatch in `update_quota` at a time: the Codex arm given
/// the Anthropic parser leaves the Codex hint at the 60-second placeholder, and the
/// Anthropic arm given the Codex parser times the Anthropic hint on a Codex window.
#[test]
fn codex_headers_time_the_codex_hint_and_never_the_anthropic_one() {
    use crate::base::provider::Provider;
    let mut codex = AccountRuntime::from_config(&account("b", 0), false);
    codex.provider = Provider::Codex;
    let manager = Manager::from_runtimes(vec![
        AccountRuntime::from_config(&account("a", 0), false),
        codex,
    ]);
    let reset = OffsetDateTime::now_utc().unix_timestamp() + 1800;
    let mut weekly_full = reqwest::header::HeaderMap::new();
    weekly_full.insert("x-codex-primary-used-percent", "100".parse().unwrap());
    weekly_full.insert("x-codex-primary-window-minutes", "10080".parse().unwrap());
    weekly_full.insert(
        "x-codex-primary-reset-at",
        reset.to_string().parse().unwrap(),
    );
    for idx in [0, 1] {
        assert!(
            !manager.update_quota(idx, &weekly_full, 200),
            "a weekly window alone is no five-hour read, for either parser"
        );
    }
    let now = OffsetDateTime::now_utc();

    let codex_hint = manager.exhaustion_hint(now, false, Provider::Codex, None);
    assert_eq!(
        codex_hint.binding,
        Some(GateReason::SevenDay),
        "the Codex account is out on its weekly window: {codex_hint:?}"
    );
    assert!(
        (1795..=1800).contains(&codex_hint.retry_after),
        "the Codex wait is the window's reset, not the 60s placeholder: {codex_hint:?}"
    );
    assert_eq!(
        codex_hint.free_at.map(OffsetDateTime::unix_timestamp),
        Some(reset)
    );

    let anthropic_hint = manager.exhaustion_hint(now, false, Provider::Anthropic, None);
    assert_eq!(
        (anthropic_hint.binding, anthropic_hint.free_at),
        (None, None),
        "the Anthropic account read nothing from Codex headers: {anthropic_hint:?}"
    );
}

/// **An all-rejected Codex fleet is timed on the windows it stored, then on its
/// holds**, never on the 60-second sentinel. `rejected` is a terminal gate with no
/// clear-instant, so with every Codex account refused the hint used to say "60" for
/// a weekly window days away. Each refused account here is out on its weekly window
/// and held at the one-hour ceiling: the hint names the SOONER stored weekly reset,
/// not the hour-long hold. With no window stored, the hold's expiry answers.
///
/// The two controls pin the fold's edges: a fleet that is not ALL rejected keeps the
/// ordinary fold (its held account frees first), and an all-rejected ANTHROPIC fleet
/// is untouched, still the sentinel.
///
/// Watched red: drop the fold after the loop and the first two answer 60; prefer the
/// hold over the stored window and the first answers the hour.
#[test]
fn an_all_rejected_codex_fleet_is_timed_on_its_stored_windows_then_its_holds() {
    use crate::base::provider::Provider;
    let now = OffsetDateTime::now_utc();
    let rejected = |name: &str, provider: Provider, weekly_reset_in: Option<i64>| {
        let mut account = AccountRuntime::from_config(&account(name, 0), false);
        account.provider = provider;
        account.quota.status = Some("rejected".to_string());
        account.quota.seven_day = weekly_reset_in.map(|secs| crate::records::quota::QuotaWindow {
            utilization: 1.0,
            reset: Some(now + Duration::seconds(secs)),
        });
        account
    };

    let fleet = Manager::from_runtimes(vec![
        rejected("a", Provider::Codex, Some(3 * 86_400)),
        rejected("b", Provider::Codex, Some(2 * 86_400)),
    ]);
    fleet.mark_rate_limited(0, 3600);
    fleet.mark_rate_limited(1, 3600);
    let hint = fleet.exhaustion_hint(now, false, Provider::Codex, None);
    assert_eq!(
        hint.free_at.map(OffsetDateTime::unix_timestamp),
        Some((now + Duration::seconds(2 * 86_400)).unix_timestamp()),
        "the sooner stored weekly reset, not the 60s sentinel nor the hour-long hold: {hint:?}"
    );
    assert_eq!(hint.binding, Some(GateReason::Rejected));
    assert_eq!(hint.gated.get(&GateReason::Rejected), Some(&2));

    let no_window = Manager::from_runtimes(vec![rejected("a", Provider::Codex, None)]);
    no_window.mark_rate_limited(0, 300);
    let hint = no_window.exhaustion_hint(OffsetDateTime::now_utc(), false, Provider::Codex, None);
    assert!(
        (299..=300).contains(&hint.retry_after) && hint.free_at.is_some(),
        "with no window stored, the hold's expiry: {hint:?}"
    );

    let mixed = Manager::from_runtimes(vec![rejected("a", Provider::Codex, Some(2 * 86_400)), {
        let mut held = AccountRuntime::from_config(&account("b", 0), false);
        held.provider = Provider::Codex;
        held
    }]);
    mixed.mark_rate_limited(1, 30);
    let hint = mixed.exhaustion_hint(OffsetDateTime::now_utc(), false, Provider::Codex, None);
    assert_eq!(
        hint.binding,
        Some(GateReason::Hold),
        "control: not every account is rejected, so the ordinary fold answers: {hint:?}"
    );
    assert!((29..=30).contains(&hint.retry_after), "{hint:?}");

    let anthropic =
        Manager::from_runtimes(vec![rejected("a", Provider::Anthropic, Some(2 * 86_400))]);
    anthropic.mark_rate_limited(0, 300);
    let hint = anthropic.exhaustion_hint(now, false, Provider::Anthropic, None);
    assert_eq!(
        (hint.retry_after, hint.free_at, hint.binding),
        (60, None, None),
        "control: an all-rejected Anthropic fleet keeps the sentinel: {hint:?}"
    );
}
