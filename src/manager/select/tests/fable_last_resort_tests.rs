//! The Fable weekly last-resort pass across its three tiers.

use super::select_test_support::{account, live_manager};
use super::*;
use crate::base::config::Account;
use crate::records::quota::QuotaWindow;
use std::collections::HashSet;
use time::Duration;

const FABLE_MODEL: &str = "claude-fable-5";

fn config_with(accounts: Vec<Account>, switch_threshold: f64) -> Config {
    Config {
        switch_threshold,
        ..Config::fixture(accounts)
    }
}

/// Set account `idx`'s `7d_oi` utilization, resetting two hours out (safely
/// in the future for the lifetime of a unit test).
fn set_seven_day_oi(manager: &Manager, idx: usize, utilization: f64) {
    let now = OffsetDateTime::now_utc();
    let mut accounts = manager
        .accounts
        .write()
        .unwrap_or_else(PoisonError::into_inner);
    accounts[idx].quota.seven_day_oi = Some(QuotaWindow {
        utilization,
        reset: Some(now + Duration::hours(2)),
    });
}

/// Absent `fableWeeklyThreshold` must byte-for-byte match today's
/// behaviour: `switchThreshold` alone decides the Fable-weekly gate. Guards
/// the default so shipping the knob does not silently change an existing
/// fleet's routing — see the bridge's evidence (ten of thirteen accounts
/// sitting at 0.95-0.98 on `7d_oi`, none of them upstream-rejected).
#[test]
fn absent_fable_weekly_threshold_matches_todays_switch_threshold_gate() {
    let manager = live_manager(config_with(vec![account("acct-a")], 0.95));
    set_seven_day_oi(&manager, 0, 0.96);
    let now = OffsetDateTime::now_utc();
    let accounts = manager
        .accounts
        .read()
        .unwrap_or_else(PoisonError::into_inner);
    assert!(
        Manager::model_blocked(
            &accounts[0],
            manager.global_threshold,
            manager.fable_weekly_threshold,
            now,
            true,
        ),
        "absent fableWeeklyThreshold must gate a Fable request exactly as \
         switchThreshold did before this knob existed"
    );
}

/// `fableWeeklyThreshold: 1.0` frees an account tcr merely PREDICTS is over
/// (0.96 against a 0.95 `switchThreshold`) but still gates one upstream has
/// GENUINELY rejected (utilization `1.0`, the only value
/// `reject_model_weekly` writes) — the whole point of the bridge: let the
/// Fable bucket run to a real rejection instead of tcr's own prediction.
#[test]
fn fable_weekly_threshold_at_one_frees_prediction_but_not_rejection() {
    let mut config = config_with(vec![account("acct-a"), account("acct-b")], 0.95);
    config.fable_weekly_threshold = Some(1.0);
    let manager = live_manager(config);
    set_seven_day_oi(&manager, 0, 0.96); // predicted-over, never rejected
    set_seven_day_oi(&manager, 1, 1.0); // genuinely rejected upstream
    let now = OffsetDateTime::now_utc();
    let accounts = manager
        .accounts
        .read()
        .unwrap_or_else(PoisonError::into_inner);
    assert!(
        !Manager::model_blocked(
            &accounts[0],
            manager.global_threshold,
            manager.fable_weekly_threshold,
            now,
            true,
        ),
        "fableWeeklyThreshold=1.0 must not gate an account tcr merely \
         predicts is over — only upstream's own rejection may"
    );
    assert!(
        Manager::model_blocked(
            &accounts[1],
            manager.global_threshold,
            manager.fable_weekly_threshold,
            now,
            true,
        ),
        "fableWeeklyThreshold=1.0 must still gate a genuine upstream rejection"
    );
}

/// `account_gate`'s `FableWeekly` push must agree with `model_blocked` on the
/// same account under both an absent and a set `fableWeeklyThreshold` — two
/// independently-resolved copies is exactly how the panel and routing can
/// drift; see [`crate::base::config::effective_fable_threshold`], the one resolver both consume.
#[test]
fn account_gate_and_model_blocked_agree_on_fable_weekly_under_both_settings() {
    for fable_weekly_threshold in [None, Some(1.0)] {
        let mut config = config_with(vec![account("acct-a")], 0.95);
        config.fable_weekly_threshold = fable_weekly_threshold;
        let manager = live_manager(config);
        set_seven_day_oi(&manager, 0, 0.96);
        let now = OffsetDateTime::now_utc();
        let now_ms = odt_to_ms(now);
        let accounts = manager
            .accounts
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        let blocked = Manager::model_blocked(
            &accounts[0],
            manager.global_threshold,
            manager.fable_weekly_threshold,
            now,
            true,
        );
        let threshold = accounts[0]
            .switch_threshold
            .unwrap_or(manager.global_threshold);
        let (gate, _) = Manager::account_gate(
            &accounts[0],
            threshold,
            manager.fable_weekly_threshold,
            now,
            now_ms,
            true,
            None,
            &HashSet::new(),
            &HashSet::new(),
        );
        assert_eq!(
            gate == GateReason::FableWeekly,
            blocked,
            "fableWeeklyThreshold={fable_weekly_threshold:?}: account_gate \
             and model_blocked must agree on whether this account is \
             Fable-gated"
        );
    }
}

/// The regression this unit exists to fix: every pooled account sits at
/// 0.96 against a 0.95 threshold (the live-fleet shape from the bridge's
/// evidence) — the OLD two-tier gate hard-skipped every one of them and
/// `select()` returned `None`, which the proxy turns into a synthesized
/// 429. The new three-tier gate must still serve the request.
#[test]
fn fleet_all_over_predicted_threshold_is_still_served() {
    let manager = live_manager(config_with(
        vec![account("acct-a"), account("acct-b"), account("acct-c")],
        0.95,
    ));
    for idx in 0..3 {
        set_seven_day_oi(&manager, idx, 0.96);
    }
    let now = OffsetDateTime::now_utc();
    let served = manager.select(
        &HashSet::new(),
        now,
        Some(FABLE_MODEL),
        None,
        "/v1/messages",
        None,
    );
    assert!(
        served.is_some(),
        "a fleet predicted-over-threshold but never rejected upstream must \
         still serve a Fable request via the last resort, not synthesize a 429"
    );
}

/// An account upstream has ACTUALLY rejected on `7d_oi` (utilization
/// `1.0`, the only value `reject_model_weekly` writes) must never be the
/// last-resort pick, even though it would otherwise sort first by having
/// the lowest priority/seq tie. The other two accounts are merely
/// predicted-over, so the pick must land on one of THEM.
#[test]
fn a_genuinely_rejected_account_is_never_the_last_resort_pick() {
    let manager = live_manager(config_with(
        vec![account("acct-a"), account("acct-b"), account("acct-c")],
        0.95,
    ));
    set_seven_day_oi(&manager, 0, 1.0); // genuinely rejected upstream
    set_seven_day_oi(&manager, 1, 0.96); // predicted-over, not rejected
    set_seven_day_oi(&manager, 2, 0.97); // predicted-over, not rejected

    let now = OffsetDateTime::now_utc();
    let served = manager
        .select(
            &HashSet::new(),
            now,
            Some(FABLE_MODEL),
            None,
            "/v1/messages",
            None,
        )
        .expect("two accounts remain usable as a last resort");
    assert_ne!(
        served, 0,
        "the account genuinely rejected upstream (utilization 1.0) must \
         never be the last-resort pick"
    );
}

/// Every account genuinely rejected upstream (`>= 1.0` on all three) — the
/// fleet really is exhausted, so `select()` must still answer `None` and
/// let the proxy's real 429 fire. The last resort must not paper over an
/// authority-confirmed exhaustion.
#[test]
fn fleet_fully_rejected_upstream_still_returns_none() {
    let manager = live_manager(config_with(
        vec![account("acct-a"), account("acct-b")],
        0.95,
    ));
    set_seven_day_oi(&manager, 0, 1.0);
    set_seven_day_oi(&manager, 1, 1.0);

    let now = OffsetDateTime::now_utc();
    let served = manager.select(
        &HashSet::new(),
        now,
        Some(FABLE_MODEL),
        None,
        "/v1/messages",
        None,
    );
    assert_eq!(
        served, None,
        "genuine exhaustion (every account rejected upstream) must still 429"
    );
}

/// A non-Fable request against the same over-predicted-threshold fleet must
/// be byte-identical to today: `is_near` already ignores `7d_oi` for
/// non-Fable traffic, so this request was never gated on it and the new
/// last-resort path (gated on `is_fable`) must never even be consulted.
#[test]
fn a_non_fable_request_is_unaffected() {
    let manager = live_manager(config_with(
        vec![account("acct-a"), account("acct-b")],
        0.95,
    ));
    set_seven_day_oi(&manager, 0, 0.96);
    set_seven_day_oi(&manager, 1, 0.97);

    let now = OffsetDateTime::now_utc();
    let served = manager.select(
        &HashSet::new(),
        now,
        Some("claude-opus-4-6"),
        None,
        "/v1/messages",
        None,
    );
    assert!(
        served.is_some(),
        "a non-Fable request must never be gated by the Fable-weekly bucket, \
         last resort or otherwise"
    );
}

/// An account still UNDER the predicted threshold is preferred over one
/// already over it — the last resort only ever widens the pool once the
/// normal picks already found nothing, it never changes the ORDER a
/// healthy fleet picks in.
#[test]
fn an_account_under_threshold_is_preferred_over_one_above_it() {
    let manager = live_manager(config_with(vec![account("fresh"), account("stale")], 0.95));
    set_seven_day_oi(&manager, 0, 0.50); // fresh: well under threshold
    set_seven_day_oi(&manager, 1, 0.96); // stale: over threshold

    let now = OffsetDateTime::now_utc();
    let served = manager
        .select(
            &HashSet::new(),
            now,
            Some(FABLE_MODEL),
            None,
            "/v1/messages",
            None,
        )
        .expect("the fresh account is eligible normally");
    assert_eq!(
        served, 0,
        "an account under the predicted threshold must be picked over one \
         above it, without ever touching the last resort"
    );
}

/// Hard constraint from the bridge: a RESERVED strict group must still
/// return `None` when its own member cannot serve, even though a last-resort
/// candidate exists elsewhere in the pool — the last resort widens ONLY the
/// whole-pool soft fallback, never a reservation, which is a privacy
/// decision, not a load-balancing one.
#[test]
fn a_reserved_strict_group_still_returns_none_with_a_last_resort_candidate_elsewhere() {
    let mut priv_account = account("gil-private");
    priv_account.groups = Some(vec!["priv".to_string()]);
    let mut config = config_with(vec![priv_account, account("pool-account")], 0.95);
    config.group_settings.insert(
        "priv".to_string(),
        config::GroupSettings {
            reserved: true,
            ..Default::default()
        },
    );
    let manager = live_manager(config);
    set_seven_day_oi(&manager, 0, 0.96); // the reserved group's only member
    set_seven_day_oi(&manager, 1, 0.50); // pool-only, would serve as a last resort

    let now = OffsetDateTime::now_utc();
    let served = manager.select_with_group(
        &HashSet::new(),
        now,
        Some(FABLE_MODEL),
        None,
        "/v1/messages",
        None,
        Some("priv"),
    );
    assert_eq!(
        served, None,
        "a reserved group's own request must refuse rather than spill to \
         the whole pool's last resort"
    );
}
