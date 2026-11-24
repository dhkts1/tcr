//! Session migration off stacked accounts onto idle ones.

use super::*;

/// Like [`config_with`] but with the (default-OFF) load-balancing migration
/// explicitly enabled — every test that exercises the migration scan itself
/// must opt in, exactly as an operator would via `~/.config/tcr.json`.
fn config_with_migration(accounts: Vec<Account>) -> Config {
    let mut config = config_with(accounts);
    config.extra.insert(
        "loadBalanceMigration".to_string(),
        serde_json::Value::Bool(true),
    );
    config
}

/// Migration #1 — a LONE session is NEVER migrated: its warm cache is preserved
/// even when an idle, eligible account sits right next to it. `count(X) < 2` must
/// be byte-identical to honouring the pin.
#[test]
fn lone_session_never_migrates() {
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    let now = OffsetDateTime::now_utc();
    // One session pinned to acct 0; acct 1 is idle and eligible.
    pin_session(&manager, 100, 0);
    for _ in 0..6 {
        assert_eq!(
            manager.select(&HashSet::new(), now, None, Some(100), "/v1/messages", None),
            Some(0),
            "a lone session must stay on its warm account (no migration)"
        );
    }
    assert_eq!(pin_of(&manager, 100), Some(0), "the pin never moved");
}

/// Migration #2 — two sessions stacked on acct 0 with acct 1 idle+eligible and
/// `loadBalanceMigration` ENABLED: selecting for one of them migrates it to
/// acct 1, and the affinity map records the new pin.
#[test]
fn stacked_session_migrates_to_idle() {
    let manager = build_manager(
        config_with_migration(vec![account("a", 0), account("b", 0)]),
        counting_refresher(),
    );
    let now = OffsetDateTime::now_utc();
    pin_session(&manager, 10, 0);
    pin_session(&manager, 11, 0);
    // count(0)=2, count(1)=0 → 0+1 < 2, so migrate this session onto acct 1.
    assert_eq!(
        manager.select(&HashSet::new(), now, None, Some(10), "/v1/messages", None),
        Some(1),
        "a stacked session migrates to the idle eligible account"
    );
    assert_eq!(
        pin_of(&manager, 10),
        Some(1),
        "the affinity map now pins the migrated session to acct 1"
    );
}

/// Migration #3 — convergence, no thrash (with `loadBalanceMigration` ENABLED):
/// after #2's migration the layout is balanced (one session each), so further
/// selects for BOTH sessions are stable — neither bounces back.
#[test]
fn migration_converges_no_thrash() {
    let manager = build_manager(
        config_with_migration(vec![account("a", 0), account("b", 0)]),
        counting_refresher(),
    );
    let now = OffsetDateTime::now_utc();
    pin_session(&manager, 10, 0);
    pin_session(&manager, 11, 0);
    // The one migration from #2.
    assert_eq!(
        manager.select(&HashSet::new(), now, None, Some(10), "/v1/messages", None),
        Some(1)
    );
    // Now 1-and-1: every further select is a lone session on its account.
    for _ in 0..5 {
        assert_eq!(
            manager.select(&HashSet::new(), now, None, Some(10), "/v1/messages", None),
            Some(1),
            "the migrated session stays put (no bounce back)"
        );
        assert_eq!(
            manager.select(&HashSet::new(), now, None, Some(11), "/v1/messages", None),
            Some(0),
            "the remaining session stays put"
        );
    }
}

/// Migration #4 — no eligible emptier account, no migration: two sessions stack on
/// acct 0 and the only other account is errored, so the stacked session honours
/// its pin rather than migrating onto a throttled/dead account.
#[test]
fn no_migration_when_no_emptier_eligible() {
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    let now = OffsetDateTime::now_utc();
    pin_session(&manager, 20, 0);
    pin_session(&manager, 21, 0);
    manager.mark_error(1); // the only alternative is ineligible
    for _ in 0..5 {
        assert_eq!(
            manager.select(&HashSet::new(), now, None, Some(20), "/v1/messages", None),
            Some(0),
            "no eligible emptier account → honour the pin, never migrate onto a dead one"
        );
    }
    assert_eq!(pin_of(&manager, 20), Some(0), "the pin never moved");
}

/// Migration #5 — with `loadBalanceMigration` ENABLED, three sessions stacked on
/// acct 0 with accts 1,2 idle spread to one-each after a round of selects
/// (convergence across three accounts).
#[test]
fn three_sessions_spread_across_three_idle() {
    let manager = build_manager(
        config_with_migration(vec![account("a", 0), account("b", 0), account("c", 0)]),
        counting_refresher(),
    );
    let now = OffsetDateTime::now_utc();
    pin_session(&manager, 1, 0);
    pin_session(&manager, 2, 0);
    pin_session(&manager, 3, 0);
    // A round of selects, one per session.
    for key in [1u64, 2, 3] {
        manager.select(&HashSet::new(), now, None, Some(key), "/v1/messages", None);
    }
    let mut homes = [
        pin_of(&manager, 1),
        pin_of(&manager, 2),
        pin_of(&manager, 3),
    ]
    .map(|p| p.expect("each session stays pinned"));
    homes.sort_unstable();
    assert_eq!(
        homes,
        [0, 1, 2],
        "three stacked sessions spread to one-per-account after a round of selects"
    );
}

/// Migration #6 — the DEFAULT is no migration at all. Identical stacked layout to
/// #2 (two sessions on acct 0, acct 1 idle and eligible): with a plain config
/// NEITHER session moves, because a session that already has a pin is by
/// definition warm and Anthropic's prompt cache is per-account, so re-pinning it
/// merely to even out counts throws that cache away. The SAME layout under
/// `"loadBalanceMigration": true` still migrates — proof the behaviour is gated,
/// not deleted.
#[test]
fn migration_is_off_by_default_and_pin_is_honoured() {
    let now = OffsetDateTime::now_utc();

    // --- default config: the migration scan never runs ---
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    assert!(
        !manager.load_balance_migration_enabled(),
        "loadBalanceMigration must default to OFF"
    );
    pin_session(&manager, 30, 0);
    pin_session(&manager, 31, 0);
    // count(0)=2, count(1)=0 — exactly the condition that used to migrate.
    for _ in 0..5 {
        assert_eq!(
            manager.select(&HashSet::new(), now, None, Some(30), "/v1/messages", None),
            Some(0),
            "a stacked session keeps its warm account when migration is off"
        );
        assert_eq!(
            manager.select(&HashSet::new(), now, None, Some(31), "/v1/messages", None),
            Some(0),
            "its stack-mate keeps the same account too"
        );
    }
    assert_eq!(pin_of(&manager, 30), Some(0), "pin 30 never moved");
    assert_eq!(pin_of(&manager, 31), Some(0), "pin 31 never moved");

    // --- same layout, key explicitly true: the old behaviour is intact ---
    let enabled = build_manager(
        config_with_migration(vec![account("a", 0), account("b", 0)]),
        counting_refresher(),
    );
    assert!(
        enabled.load_balance_migration_enabled(),
        "the explicit key must read back as ON"
    );
    pin_session(&enabled, 30, 0);
    pin_session(&enabled, 31, 0);
    assert_eq!(
        enabled.select(&HashSet::new(), now, None, Some(30), "/v1/messages", None),
        Some(1),
        "with loadBalanceMigration=true the stacked session still migrates"
    );
    assert_eq!(
        pin_of(&enabled, 30),
        Some(1),
        "and the affinity map records the migrated pin"
    );
}

/// TOCTOU regression (concurrency) — the migration feature's OWN target workload,
/// with `loadBalanceMigration` explicitly ENABLED (it ships OFF).
/// Several sessions start STACKED on one account with idle, eligible accounts
/// alongside. `N` threads select for those sessions in lockstep: a `Barrier`
/// forces every round to fire simultaneously, which is the exact interleaving the
/// adversarial review flagged — two concurrent selects both reading `count(Y)=0`
/// for the same idle `Y` and BOTH migrating onto it, over-stacking `Y` to 2 while
/// `X` empties to 0 (the inverse of the goal) and oscillating 0→1→0.
///
/// The fix — re-validating `count(target)+1 < count(X)` against FRESH counts under
/// the affinity lock in section 3, aborting the move if it no longer holds — makes
/// each committed migration strictly reduce the load's sum-of-squares, so the
/// system converges to exactly one session per account and STAYS there. Under the
/// pre-fix code the stale-count commit over-migrates and the final distribution is
/// NOT balanced. (Asserted on a balanced fleet — no pacing / rate-limit churn — so
/// the only force acting on the distribution is the migration logic under test.)
#[test]
fn concurrent_stacked_sessions_do_not_over_migrate() {
    const N: usize = 4; // sessions == accounts → balanced fixed point is one-each
    const ROUNDS: usize = 100;

    let manager = build_manager(
        config_with_migration(vec![
            account("a0", 0),
            account("a1", 0),
            account("a2", 0),
            account("a3", 0),
        ]),
        counting_refresher(),
    );
    // Pathological start: ALL N sessions pinned to account 0.
    for key in 0..N as u64 {
        pin_session(&manager, key, 0);
    }

    let barrier = Arc::new(std::sync::Barrier::new(N));
    let handles: Vec<_> = (0..N as u64)
        .map(|key| {
            let manager = Arc::clone(&manager);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let empty: HashSet<usize> = HashSet::new();
                for _ in 0..ROUNDS {
                    // Synchronise so all N selects contend on the same stacked
                    // account in the same instant — maximising the TOCTOU window.
                    barrier.wait();
                    let now = OffsetDateTime::now_utc();
                    manager.select(&empty, now, None, Some(key), "/v1/messages", None);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().expect("migration stress thread panicked");
    }

    // Every session is still pinned (none lost), and the distribution converged
    // to exactly one-per-account. An account holding >=2 while another sits at 0
    // is the over-migration / oscillation the re-validation exists to prevent.
    let pins = manager.affinity.lock().expect("affinity lock poisoned");
    assert_eq!(pins.len(), N, "every session must remain pinned");
    let mut per_account = [0usize; N];
    for &(idx, _) in pins.values() {
        per_account[idx] += 1;
    }
    assert_eq!(
        per_account,
        [1, 1, 1, 1],
        "stacked sessions must converge to one-per-account without over-migrating \
         (got {per_account:?}) — a stale-count commit would over-stack one account"
    );
}
