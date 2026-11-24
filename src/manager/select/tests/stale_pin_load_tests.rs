//! A pin's last-touch decides whether it counts as LOAD on its account
//! ([`PIN_LOAD_IDLE_MS`]), never whether it is honoured. Field report: 100
//! pins with one touched in the last hour made the emptiest account read as
//! carrying eight sessions, and every session that landed on it was
//! migrated straight back off.
use super::select_test_support::{account, live_manager, pin, pin_of};
use super::*;
use std::collections::HashSet;

/// Two accounts, `loadBalanceMigration` ON (the only mode that counts pins).
fn build_manager() -> Arc<Manager> {
    let mut extra = serde_json::Map::new();
    extra.insert(
        "loadBalanceMigration".to_string(),
        serde_json::Value::Bool(true),
    );
    let config = Config {
        extra,
        ..Config::fixture(vec![
            account("alice@example.com"),
            account("bob@example.com"),
        ])
    };
    live_manager(config)
}

/// Five days stale, the shape of the field report.
const DAYS_STALE_MS: i64 = 5 * 24 * 60 * 60 * 1000;

#[test]
fn the_window_boundary_and_the_requesting_session() {
    let now_ms = 10 * PIN_LOAD_IDLE_MS;
    assert!(pin_counts_as_load(false, now_ms, now_ms), "just touched");
    assert!(
        pin_counts_as_load(false, now_ms - PIN_LOAD_IDLE_MS, now_ms),
        "exactly at the window still counts"
    );
    assert!(
        !pin_counts_as_load(false, now_ms - PIN_LOAD_IDLE_MS - 1, now_ms),
        "one millisecond past the window does not"
    );
    assert!(
        pin_counts_as_load(true, now_ms - DAYS_STALE_MS, now_ms),
        "the requesting session is live however old its previous touch"
    );
}

/// Brief test 1. Account 0 carries six days-dead pins plus the requesting
/// session; account 1 carries one fresh pin. Counting raw pins reads 7 vs 1
/// and migrates; counting live load reads 1 vs 1 and must not.
#[test]
fn stale_pins_do_not_make_an_account_read_as_busy() {
    let manager = build_manager();
    let now = OffsetDateTime::now_utc();
    let now_ms = odt_to_ms(now);
    for key in 1..=6 {
        pin(&manager, key, 0, now_ms - DAYS_STALE_MS);
    }
    pin(&manager, 100, 1, now_ms - 60_000);
    pin(&manager, 50, 0, now_ms - 60_000);

    assert_eq!(
        manager.select(&HashSet::new(), now, None, Some(50), "/v1/messages", None),
        Some(0),
        "days-dead pins must not make account 0 look busier than account 1"
    );
    assert_eq!(
        pin_of(&manager, 50),
        Some(0),
        "the session was not migrated"
    );
}

/// Brief test 2. The same shape with the other pins FRESH is real load:
/// 3 live on account 0 vs 0 on account 1 still migrates.
#[test]
fn fresh_pins_still_count_as_load() {
    let manager = build_manager();
    let now = OffsetDateTime::now_utc();
    let now_ms = odt_to_ms(now);
    pin(&manager, 1, 0, now_ms - 60_000);
    pin(&manager, 2, 0, now_ms - (PIN_LOAD_IDLE_MS - 60_000));
    pin(&manager, 50, 0, now_ms - 60_000);

    assert_eq!(
        manager.select(&HashSet::new(), now, None, Some(50), "/v1/messages", None),
        Some(1),
        "pins inside the window are live load, so the stacked session moves"
    );
    assert_eq!(pin_of(&manager, 50), Some(1));
}

/// The re-check under the commit lock must use the same filter as the
/// decision. Account 1 carries five days-dead pins: the decision reads it
/// empty and picks it, and an unfiltered re-check (5+1 < 3 is false) would
/// silently abort the move the decision correctly made.
#[test]
fn stale_pins_on_the_target_do_not_abort_a_migration() {
    let manager = build_manager();
    let now = OffsetDateTime::now_utc();
    let now_ms = odt_to_ms(now);
    pin(&manager, 1, 0, now_ms - 60_000);
    pin(&manager, 2, 0, now_ms - 60_000);
    pin(&manager, 50, 0, now_ms - 60_000);
    for key in 11..=15 {
        pin(&manager, key, 1, now_ms - DAYS_STALE_MS);
    }

    assert_eq!(
        manager.select(&HashSet::new(), now, None, Some(50), "/v1/messages", None),
        Some(1),
        "the commit-time re-check must not count the target's dead pins"
    );
    assert_eq!(pin_of(&manager, 50), Some(1));
}

/// A count filter, not a TTL: no stale pin is removed from the map. The
/// session returning after days is a different question: its cache is cold
/// on every account, so its pin is not honoured and it takes the normal pick
/// (`a_cold_pin_takes_the_reset_order_again`), here the emptier account 0,
/// and is re-pinned there. Every OTHER stale pin is left exactly as it was.
#[test]
fn a_stale_pin_takes_the_pick_and_no_other_pin_is_dropped() {
    let manager = build_manager();
    let now = OffsetDateTime::now_utc();
    let now_ms = odt_to_ms(now);
    for key in 1..=6 {
        pin(&manager, key, 1, now_ms - DAYS_STALE_MS);
    }
    pin(&manager, 50, 1, now_ms - DAYS_STALE_MS);

    let served = manager.select(&HashSet::new(), now, None, Some(50), "/v1/messages", None);
    assert!(served.is_some(), "the returning session is served");
    let pins = manager
        .affinity
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    assert_eq!(pins.len(), 7, "no stale pin was evicted by the filter");
    assert_eq!(
        pins.get(&50).map(|&(i, _)| i),
        served,
        "the returning session is re-pinned where the pick served it"
    );
    assert!((1..=6).all(|k| pins.get(&k).map(|&(i, _)| i) == Some(1)));
}
