//! The reset-urgency bucket spends a sooner weekly reset ahead of the LRU order.

use super::select_test_support::{account, live_manager, pin};
use super::*;
use crate::base::config::Account;
use crate::records::quota::QuotaWindow;
use std::collections::HashSet;

fn config_with(accounts: Vec<Account>, tier_hours: u32) -> Config {
    Config {
        reset_urgency_tier_hours: tier_hours,
        ..Config::fixture(accounts)
    }
}

/// Give account `idx` a weekly window resetting `hours` from `now` at a
/// utilization well under the fixture's 0.90 threshold, so the account is
/// ranked but never gated — this suite is about ORDER, and an account held
/// out by quota would prove nothing about it.
fn set_weekly(manager: &Manager, idx: usize, hours: i64, now: OffsetDateTime) {
    let mut accounts = manager
        .accounts
        .write()
        .unwrap_or_else(PoisonError::into_inner);
    accounts[idx].quota.seven_day = Some(QuotaWindow {
        utilization: 0.10,
        reset: Some(now + time::Duration::hours(hours)),
    });
}

fn set_seq(manager: &Manager, idx: usize, seq: u64) {
    let mut accounts = manager
        .accounts
        .write()
        .unwrap_or_else(PoisonError::into_inner);
    accounts[idx].last_selected_seq = seq;
}

fn pick(manager: &Manager, now: OffsetDateTime) -> usize {
    manager
        .select(&HashSet::new(), now, None, None, "/v1/messages", None)
        .expect("an account is eligible")
}

/// **The discriminating test.** Two accounts in one priority tier: `urgent`
/// resets in 2h (bucket 0 at the default 24h width), `roomy` in 100h
/// (bucket 4). `urgent` is stamped as the MOST recently selected account and
/// `roomy` as never-selected, so pre-tier LRU would pick `roomy` — this test
/// fails on the old ordering, which is the point of it.
///
/// Mirrors the live fleet that motivated the change (measured 2026-08-26):
/// 13 accounts all at priority 0, resets spread 2.5h–152.5h, and the
/// soonest-resetting account sitting on 44% of a weekly bucket that was
/// worth nothing a few hours later.
#[test]
fn a_sooner_resetting_account_outranks_a_less_recently_selected_one() {
    let manager = live_manager(config_with(vec![account("urgent"), account("roomy")], 24));
    let now = OffsetDateTime::now_utc();

    set_weekly(&manager, 0, 2, now);
    set_weekly(&manager, 1, 100, now);
    // Bias plain LRU as hard as possible TOWARD `roomy`: `urgent` looks
    // most-recently-selected, `roomy` never-selected.
    set_seq(&manager, 0, 999);
    set_seq(&manager, 1, 0);

    assert_eq!(
        pick(&manager, now),
        0,
        "the account whose weekly window resets in 2h must be spent before \
         the one with 100h left, even though it is the most recently \
         selected of the two — its unused headroom is the only headroom \
         about to expire"
    );
}

/// The shape reported 2026-11-07: a session pinned to `roomy` (reset in 86h)
/// comes back after its cache went cold, and `urgent` (reset in 11h) sits
/// unused. Both are under the threshold, so before this change the pin was
/// honoured and `urgent`'s week expired unspent. A cold pin protects nothing,
/// so the request takes the reset order like any new one, and the session is
/// re-pinned where it lands.
#[test]
fn a_cold_pin_takes_the_reset_order_again() {
    let manager = live_manager(config_with(vec![account("roomy"), account("urgent")], 24));
    let now = OffsetDateTime::now_utc();
    set_weekly(&manager, 0, 86, now);
    set_weekly(&manager, 1, 11, now);
    let key = 4242;
    let cold_ms = odt_to_ms(now) - WarmWindow::DEFAULT.ms() - 1_000;
    pin(&manager, key, 0, cold_ms);

    let served = manager.select(&HashSet::new(), now, None, Some(key), "/v1/messages", None);
    assert_eq!(
        served,
        Some(1),
        "a session idle past its warm window has no cache left on `roomy`, so it \
         must take the reset order and land on the account resetting in 11h"
    );
    assert_eq!(
        manager.affinity_pin(key),
        Some(1),
        "the session is re-pinned where the pick sent it"
    );
}

/// The complement, so the rule above cannot widen: inside the warm window the
/// cache on `roomy` is real, and the pin still outranks the reset order.
#[test]
fn a_warm_pin_keeps_its_account_over_the_reset_order() {
    let manager = live_manager(config_with(vec![account("roomy"), account("urgent")], 24));
    let now = OffsetDateTime::now_utc();
    set_weekly(&manager, 0, 86, now);
    set_weekly(&manager, 1, 11, now);
    let key = 4243;
    pin(&manager, key, 0, odt_to_ms(now) - 10_000);

    let served = manager.select(&HashSet::new(), now, None, Some(key), "/v1/messages", None);
    assert_eq!(
        served,
        Some(0),
        "a warm pin keeps its cache: it is honoured over the reset order"
    );
    assert_eq!(
        manager.affinity_pin(key),
        Some(0),
        "the warm pin does not move"
    );
}

/// The other half of the design: bucketing must NOT collapse into a
/// deterministic pin. Two accounts resetting 2h apart share bucket 0 at the
/// default 24h width, so LRU still decides between them and consecutive
/// picks alternate — the fan-out that ranking on the raw reset instant would
/// have destroyed.
#[test]
fn accounts_in_one_bucket_still_fan_out_by_lru() {
    let manager = live_manager(config_with(vec![account("first"), account("second")], 24));
    let now = OffsetDateTime::now_utc();

    set_weekly(&manager, 0, 2, now);
    set_weekly(&manager, 1, 4, now);

    let picks: Vec<usize> = (0..4).map(|_| pick(&manager, now)).collect();
    assert!(
        picks.contains(&0) && picks.contains(&1),
        "two accounts sharing one urgency bucket must both be served — \
         got {picks:?}, which is the deterministic pin the bucket exists \
         to prevent"
    );
}

/// `resetUrgencyTierHours: 0` is the config-only rollback: the urgency term
/// ties for every account and the pre-tier LRU order returns. Same fixture
/// as the discriminating test, opposite expected winner.
#[test]
fn tier_hours_zero_restores_the_pre_tier_lru_order() {
    let manager = live_manager(config_with(vec![account("urgent"), account("roomy")], 0));
    let now = OffsetDateTime::now_utc();

    set_weekly(&manager, 0, 2, now);
    set_weekly(&manager, 1, 100, now);
    set_seq(&manager, 0, 999);
    set_seq(&manager, 1, 0);

    assert_eq!(
        pick(&manager, now),
        1,
        "with the tier term disabled the never-selected account wins on \
         plain LRU, exactly as it did before this feature existed"
    );
}

/// An account with no known live weekly reset keeps sorting FIRST — the
/// pre-tier "unknown quota → probe it" contract, which the bucket must
/// carry through rather than quietly re-rank. `roomy` here is both
/// never-selected AND resetting soon; the unmeasured account still beats it.
#[test]
fn an_unknown_weekly_reset_still_sorts_ahead_of_every_bucket() {
    let manager = live_manager(config_with(
        vec![account("unmeasured"), account("soon")],
        24,
    ));
    let now = OffsetDateTime::now_utc();

    // `unmeasured` deliberately gets NO weekly window at all.
    set_weekly(&manager, 1, 1, now);
    set_seq(&manager, 0, 999);
    set_seq(&manager, 1, 0);

    assert_eq!(
        pick(&manager, now),
        0,
        "an account whose weekly window was never measured must be picked \
         (and so probed) ahead of every bucketed account"
    );
}

/// Unit-level check of the bucket arithmetic itself, independent of
/// selection: the floor division, the disabled arm, and the unknown-reset
/// sentinel. Widths are checked at the boundary (24h lands in bucket 1, not
/// 0) because an off-by-one here silently re-ranks the whole fleet.
#[test]
fn bucket_arithmetic_floors_and_handles_both_sentinels() {
    let manager = live_manager(config_with(vec![account("a")], 24));
    let now = OffsetDateTime::now_utc();

    let tier = |hours: i64| {
        set_weekly(&manager, 0, hours, now);
        let accounts = manager
            .accounts
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        manager.reset_urgency_tier(&accounts[0], now)
    };

    assert_eq!(tier(2), 0, "2h into a 24h width is bucket 0");
    assert_eq!(tier(23), 0, "23h is still bucket 0");
    assert_eq!(tier(25), 1, "25h crosses into bucket 1");
    assert_eq!(tier(100), 4, "100h floors to bucket 4");
    assert_eq!(tier(152), 6, "the live fleet's furthest account, bucket 6");

    // Unknown reset → the minimum, ahead of every bucket.
    {
        let mut accounts = manager
            .accounts
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        accounts[0].quota.seven_day = None;
    }
    {
        let accounts = manager
            .accounts
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        assert_eq!(
            manager.reset_urgency_tier(&accounts[0], now),
            i64::MIN,
            "an unmeasured weekly window sorts ahead of every bucket"
        );
    }

    // Disabled → a constant tie for everyone, including the unknown arm.
    let off = live_manager(config_with(vec![account("a")], 0));
    {
        let accounts = off.accounts.read().unwrap_or_else(PoisonError::into_inner);
        assert_eq!(
            off.reset_urgency_tier(&accounts[0], now),
            0,
            "a disabled tier term must tie, not rank"
        );
    }
}
