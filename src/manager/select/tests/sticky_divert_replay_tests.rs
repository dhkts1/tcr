//! A replayed four-divert episode keeps landing on its one sticky alternate.

use super::select_test_support::{account, config_with, live_manager};
use super::*;
use std::collections::HashSet;

/// Four `select()` calls in a row for one pinned, rate-limited session —
/// the exact shape of the retained trace (four diverts, `pin-tried`
/// every time because the pin is already in the request's own `tried`
/// set after the first attempt upstream, matching the real log's
/// `reason=pin-tried` on all four lines). Every call after the first
/// must return the SAME alternate: that reuse is the entire point of
/// PR #110, and its absence (a fresh least-utilised pick every time) is
/// the bounce the production trace shows.
#[test]
fn four_diverts_in_one_episode_reuse_the_same_alternate() {
    let manager = live_manager(config_with(vec![
        account("alice"),
        account("bob"),
        account("carol"),
    ]));
    let now = OffsetDateTime::now_utc();
    let key = 42_u64;

    // Initial pin, mirroring the trace: the session pins to `alice`,
    // which then gets rate-limited.
    let pin = manager
        .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
        .expect("an account is eligible for the initial pin");
    manager.mark_rate_limited(pin, 120);

    // Every one of the four replayed diverts hits the SAME upstream
    // gate the log's `reason=pin-tried` names: the pin already failed
    // this request (it is in `tried`), so `select()` diverts and keeps
    // the pin rather than re-keying the session.
    let mut tried = HashSet::new();
    tried.insert(pin);

    let first = manager
        .select(&tried, now, None, Some(key), "/v1/messages", None)
        .expect("an alternate is eligible for the first divert");
    assert_ne!(
        first, pin,
        "the first divert must land on an alternate, not the held pin"
    );

    for n in 2..=4 {
        let dest = manager
            .select(&tried, now, None, Some(key), "/v1/messages", None)
            .unwrap_or_else(|| panic!("an alternate is eligible for divert #{n}"));
        assert_eq!(
            dest, first,
            "divert #{n} must reuse the episode's sticky destination {first}, \
             not bounce to a different alternate — this is the exact shape \
             of the production trace this test replays"
        );
    }

    // The episode ledger backs the reuse: one recorded episode, keyed
    // on this session, with exactly one distinct destination touched
    // across all four diverts.
    let episode = manager
        .divert_ledger
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&key)
        .copied()
        .expect("episode recorded");
    assert_eq!(episode.pin, pin);
    assert_eq!(episode.sticky, first);
    assert_eq!(
        episode.destinations.count_ones(),
        1,
        "four diverts reusing the same destination is one distinct \
         destination, not four"
    );

    // Episode reset is structural, not timed (§4.1): a NEW rate-limit
    // deadline on the same pin changes the episode's identity
    // `(pin, until_ms)`, so the next divert is `Fresh` again and is free
    // to land on any eligible alternate — including a different one.
    // With `first` now the most-recently-selected account (its
    // `last_selected_seq` is ahead of the untouched third account), the
    // LRU-ordered normal pick this fixture exercises actually lands on
    // the OTHER alternate, making the "allowed to change" claim
    // concrete rather than vacuous.
    manager.mark_rate_limited(pin, 200);
    let after_reset = manager
        .select(&tried, now, None, Some(key), "/v1/messages", None)
        .expect("an alternate is eligible after the episode resets");
    assert_ne!(
        after_reset, first,
        "a new rate-limit deadline is a new episode (§4.1) — the sticky \
         destination must be free to change, and with an untouched \
         third account in the fleet the LRU pick actually does change it"
    );
}
