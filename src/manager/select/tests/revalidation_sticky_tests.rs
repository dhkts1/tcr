//! `select_revalidation`'s fallback reuses the divert episode's sticky destination.

use super::select_test_support::{account, config_with, live_manager};
use super::*;
use std::collections::HashSet;

/// The headline claim: after `select()` diverts and records this
/// episode's sticky destination in `Manager::divert_ledger`, a request
/// that reaches `select_revalidation`'s FALLBACK arm for the SAME
/// episode must reuse that same destination — not the fleet's
/// least-utilised account, which (three accounts, mirroring B's own
/// anti-churn-defeating fixture) would otherwise be the OTHER alternate.
#[test]
fn revalidation_fallback_reuses_the_episode_sticky_destination() {
    let manager = live_manager(config_with(vec![
        account("home"),
        account("alt1"),
        account("alt2"),
    ]));
    let now = OffsetDateTime::now_utc();
    let key = 636_363u64;

    let home = manager
        .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
        .expect("an account is eligible");
    // A long hold: outlives the soft-wait ceiling in the real ladder
    // (§1.2), which is exactly the case this design's §1.2 "revalidation
    // leak" is about — but `select_revalidation` itself takes no ceiling,
    // so any hold that keeps `home` HARD-ok-but-rate-limited exercises
    // the same PIN-HONOR-falls-through-to-FALLBACK path.
    manager.mark_rate_limited(home, 120);

    // `select()` diverts and records the episode's sticky destination —
    // this is the ledger entry `select_revalidation` must now consult.
    let sticky_dest = manager
        .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
        .expect("an un-held alternate serves this divert");
    assert_ne!(sticky_dest, home);

    // `select_revalidation`'s FALLBACK must reuse `sticky_dest`, not the
    // OTHER un-held alternate — the assertion three accounts (not two)
    // makes real, per B's own fixture comment.
    let served = manager
        .select_revalidation(&HashSet::new(), now, None, Some(key), "/v1/messages")
        .expect("an un-held alternate serves this revalidation request");
    assert_eq!(
        served, sticky_dest,
        "select_revalidation's FALLBACK must consult the same episode \
         ledger `select()` populated, reusing its sticky destination \
         instead of spending a fresh least-utilised account"
    );

    // The episode ledger must record the FALLBACK's own serve too (§4.2:
    // "every divert records, not just sticky ones"), same destination,
    // still one distinct destination.
    let episode = manager
        .divert_ledger
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&key)
        .copied()
        .expect("episode recorded");
    assert_eq!(episode.pin, home);
    assert_eq!(episode.sticky, sticky_dest);
    assert_eq!(
        episode.destinations.count_ones(),
        1,
        "select() and select_revalidation reusing the SAME destination \
         is one distinct destination, not two"
    );
}
