//! Every door out of a strict group stays shut.

use super::select_test_support::{live_manager, pin, pin_of};
use super::*;
use crate::base::config::Account;
use std::collections::HashSet;

fn account(name: &str, groups: &[&str]) -> Account {
    Account {
        groups: Some(groups.iter().map(|g| (*g).to_string()).collect()),
        ..Account::fixture(name)
    }
}

fn config_with(accounts: Vec<Account>, lock_account: Option<String>) -> Config {
    Config {
        lock_account,
        ..Config::fixture(accounts)
    }
}

/// `config_with` plus a named control account, the door §H the
/// control-preferred overlay below needs to reach.
fn config_with_control(accounts: Vec<Account>, control_name: &str) -> Config {
    let mut config = config_with(accounts, None);
    config.control_account = Some(config::ControlAccountRef::Name(control_name.to_string()));
    config
}

/// **A HARD ACCOUNT LOCK DOES NOT BEAT A STRICT GROUP**, and this is the
/// door that opened before the ask was even computed.
///
/// `lockAccount` used to return above `strict_group`, so a lender with one
/// set served every borrowed request on the locked account whatever the
/// lease was scoped to. `None` and not a widening: a lock means "this
/// account and no other", a strict ask means "inside this group and nowhere
/// else", and when they disagree no account satisfies both.
///
/// Watched red: move the `locked_idx` arm back above `strict_group` (or
/// delete its `in_group` check) and the first assertion answers
/// `Some(bob)`.
#[test]
fn a_hard_account_lock_is_refused_outside_a_strict_group() {
    let manager = live_manager(config_with(
        vec![account("alice", &["work"]), account("bob", &["home"])],
        Some("bob".to_string()),
    ));
    let now = OffsetDateTime::now_utc();
    let locked = manager
        .select(&HashSet::new(), now, None, None, "/v1/messages", None)
        .expect("the lock always answers when nothing asks for a group");

    assert_eq!(
        manager.select_with_group(
            &HashSet::new(),
            now,
            None,
            None,
            "/v1/messages",
            None,
            Some("work"),
        ),
        None,
        "the locked account is in `home`, so a STRICT ask for `work` has no \
         answer, and must not be served on the lock"
    );

    // The controls, without which the refusal above could be a lock that
    // simply stopped working.
    assert_eq!(
        manager.select_with_group(
            &HashSet::new(),
            now,
            None,
            None,
            "/v1/messages",
            None,
            Some("home"),
        ),
        Some(locked),
        "a strict ask the lock SATISFIES is still served on the lock"
    );
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
        Some(locked),
        "and a request that asks for no group is unaffected"
    );
}

/// **A STICKY DIVERT DESTINATION DOES NOT BEAT A STRICT GROUP**, the hunk
/// added later and nobody covered: reverting it left the suite green.
///
/// Stickiness exists so a diverted session keeps landing on ONE warm
/// alternative, and for a spill (PREFER-only) group it still bypasses the
/// narrowing exactly as before. A strict ask is not a preference to bypass.
///
/// Watched red: pass `None` instead of `strict_group` to `Self::eligible`
/// in `select_with_group`'s `sticky_pick` block and the strict ask below is
/// answered `Some(sticky)`, an account outside the group, one divert
/// later.
#[test]
fn a_sticky_divert_destination_is_refused_outside_a_strict_group() {
    let manager = live_manager(config_with(
        vec![account("alice", &["a-team"]), account("bob", &["b-team"])],
        None,
    ));
    let now = OffsetDateTime::now_utc();
    let key = 7_u64;

    // One pinned session, its pin then rate-limited, so the next request
    // for that session DIVERTS and keeps the pin.
    let pin = manager
        .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
        .expect("an account is eligible for the initial pin");
    manager.mark_rate_limited(pin, 120);
    let mut tried = HashSet::new();
    tried.insert(pin);

    // The first divert records the episode, and its destination becomes
    // the sticky one. This is the live sticky path, not a seeded ledger.
    let sticky = manager
        .select(&tried, now, None, Some(key), "/v1/messages", None)
        .expect("the other account serves the first divert");
    assert_ne!(sticky, pin, "a divert lands on an alternate");
    assert_eq!(
        manager
            .divert_ledger
            .lock()
            .expect("divert ledger lock poisoned")
            .get(&key)
            .copied()
            .expect("the episode is recorded")
            .sticky,
        sticky,
        "the sticky destination is the one the strict ask below must refuse"
    );

    // THE SUBJECT. The pin's OWN group is the strict ask: its only member
    // is the pin, which this request has already tried, so the honest
    // answer is `None`. The sticky destination is in the other group, and
    // serving it would be the out-of-scope serve.
    let pin_group = {
        let accounts = manager
            .accounts
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        accounts[pin].groups[0].clone()
    };
    let answer = manager.select_with_group(
        &tried,
        now,
        None,
        Some(key),
        "/v1/messages",
        None,
        Some(&pin_group),
    );
    assert_ne!(
        answer,
        Some(sticky),
        "the sticky destination is outside the strictly-asked group {pin_group} \
         and must never be the answer"
    );
    assert_eq!(
        answer, None,
        "and the honest answer is the refusal a strict ask is owed"
    );

    // The control: the SAME session, the same episode, the same sticky
    // destination, with no group asked for. Without this the assertion
    // above could be passing because stickiness stopped working.
    assert_eq!(
        manager.select(&tried, now, None, Some(key), "/v1/messages", None),
        Some(sticky),
        "with no group asked for, the sticky destination is still reused"
    );
}

/// **`is_group_strict` is `reserved || !spill`**, the one rule, so
/// `src/proxy.rs`'s refusal terminals scope their answer to the same set
/// the picker held the request inside.
///
/// Watched red: make `is_group_strict` call `is_group_reserved` alone and
/// the first assertion fails, which is exactly the state the two
/// `proxy.rs` call sites were in.
#[test]
fn a_group_that_is_neither_reserved_nor_spill_is_strict() {
    let manager = live_manager(config_with(vec![account("alice", &["work"])], None));
    assert!(
        manager.is_group_strict("work"),
        "a plain group is strict: it is not a spill group, so the picker \
         holds its traffic inside it"
    );
    assert!(
        !manager.is_group_spill("work"),
        "and it is not a spill group, which is what makes it strict"
    );
    assert!(
        manager.is_group_strict("never-configured"),
        "a group nobody configured is strict too, for the same reason"
    );
}

/// `config_with` plus `loadBalanceMigration: true`, the only mode the
/// migration scan below runs under (`Self::load_balance_migration_enabled`).
fn config_with_migration(accounts: Vec<Account>) -> Config {
    let mut config = config_with(accounts, None);
    let mut extra = serde_json::Map::new();
    extra.insert(
        "loadBalanceMigration".to_string(),
        serde_json::Value::Bool(true),
    );
    config.extra = extra;
    config
}

// The tests below seed LIVE pins directly with `select_test_support::pin`,
// bypassing `select`, so two sessions can be stacked on one account
// deterministically: a fresh, unpinned `select` alternates its LRU pick across
// every eligible account (`Self::pick_eligible`'s `last_selected_seq`
// tie-break), so two back-to-back calls do NOT reliably land on the same index
// the way the migration scan's precondition (`count_x >= 2`) needs.

/// **THE FOURTH DOOR OUT OF A STRICT GROUP**, and the one that returns
/// from inside the affinity fast path, so neither the hard-lock test above
/// nor the sticky-destination test above ever reaches it.
///
/// Watched red: the migration scan's `Self::eligible` call at this file's
/// `:901` passes `None` for the group PREFERENCE instead of `strict_group`,
/// so `at-outside` (no groups at all) is "eligible" for a strict `shared`
/// ask and, being the only unpinned account, is the least-loaded migration
/// target, `served` comes back `1`, the `assert_ne!` below fails, and the
/// session is committed and re-pinned there (`:999-1004`). Green once that
/// argument is `strict_group`.
#[test]
fn a_migration_target_is_refused_outside_a_strict_group() {
    let manager = live_manager(config_with_migration(vec![
        account("at-inside", &["shared"]),
        account("at-outside", &[]),
    ]));
    let now = OffsetDateTime::now_utc();
    let now_ms = odt_to_ms(now);
    // Two sessions already stacked on the in-group account: `count_x == 2`,
    // so the migration scan runs on the very next select for either one.
    pin(&manager, 10, 0, now_ms);
    pin(&manager, 11, 0, now_ms);

    let served = manager
        .select_with_group(
            &HashSet::new(),
            now,
            None,
            Some(10),
            "/v1/messages",
            None,
            Some("shared"),
        )
        .expect("the strict group still has a servable member");
    assert_ne!(
        served, 1,
        "`at-outside` carries no `shared` group; a migration target \
         outside the strict group is the out-of-scope serve the scope \
         exists to prevent"
    );
    assert_eq!(
        pin_of(&manager, 10),
        Some(served),
        "and whatever it chose is what the session is pinned to"
    );
}

/// The migration scan is the one candidate loop in this file with no
/// `tried` guard.
///
/// Watched red: `:891`'s `continue` fires only for `cand == idx`, so
/// `at-b`, already in `tried`, either the account a borrowed request's
/// scope benched (`src/proxy.rs:2249-2256`) or the account a local retry
/// just failed on, is still the least-loaded candidate and the scan
/// returns it, migrating the session onto the very account `tried`
/// exists to rule out. Green once `:891` also `continue`s on
/// `tried.contains(&cand)`.
#[test]
fn a_migration_target_is_never_an_account_the_request_already_benched() {
    let manager = live_manager(config_with_migration(vec![
        account("at-a", &[]),
        account("at-b", &[]),
    ]));
    let now = OffsetDateTime::now_utc();
    let now_ms = odt_to_ms(now);
    // Two sessions already stacked on `at-a`: `count_x == 2`, so the scan
    // runs and `at-b` (empty, and therefore the least-loaded candidate)
    // is the only place it could migrate to.
    pin(&manager, 10, 0, now_ms);
    pin(&manager, 11, 0, now_ms);

    // What a borrowed request under `x-tcr-accounts: at-a` looks like by
    // the time it reaches the picker (`src/proxy.rs:2249-2256`), and
    // equally what a local retry looks like after `at-b` failed it.
    let mut tried = HashSet::new();
    tried.insert(1);
    assert_ne!(
        manager.select(&tried, now, None, Some(10), "/v1/messages", None),
        Some(1),
        "`tried` is the set the picker may not return, and the migration \
         scan is inside the picker"
    );
}

/// **THE CONTROL-PREFERRED OVERLAY NEEDS NO CONFIG FLAG** to leave a
/// strict group, only an unpinned session, a non-inference path, and a
/// configured `controlAccount`.
///
/// Watched red: `Self::control_eligible` deliberately bypasses every
/// narrowing gate including group membership (see its own doc), and the
/// overlay at this file's `:1080-1102` called it alone, so an unpinned
/// `/v1/organizations`-class request asking strictly for `shared` was
/// routed onto `at-control`, which carries no `shared` group. Green once
/// the overlay also requires `strict_group` membership.
#[test]
fn control_preferred_overlay_is_refused_outside_a_strict_group() {
    let manager = live_manager(config_with_control(
        vec![
            account("at-inside", &["shared"]),
            account("at-control", &[]),
        ],
        "at-control",
    ));
    let now = OffsetDateTime::now_utc();

    let served = manager
        .select_with_group(
            &HashSet::new(),
            now,
            None,
            None, // unpinned, the overlay only ever runs on a fresh session
            "/v1/organizations",
            None,
            Some("shared"),
        )
        .expect("the strict group still has a servable member");
    assert_ne!(
        served, 1,
        "`at-control` carries no `shared` group; the control-preferred \
         overlay is a designated-identity route, not a licence to leave \
         a strict group with no config flag at all"
    );

    // The control: with no group asked for, the same request still
    // prefers the control account, the overlay itself is unaffected.
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, "/v1/organizations", None),
        Some(1),
        "with no group asked for, control-preferred routing is unchanged"
    );
}

/// **THE CONNECTION-AFFINITY (NOISE) OVERLAY NEEDS NO CONFIG FLAG
/// EITHER**, the reachability turns only on the lender's HTTP client
/// reusing one loopback connection across leases (unmeasured by the
/// review), never on `loadBalanceMigration`.
///
/// Watched red: the overlay at this file's `:1034-1075` called
/// `Self::eligible` with `group: None`, so a `/mcp-registry`-class
/// request whose connection was already following `at-outside` (no
/// `shared` group) kept following it even under a strict `shared` ask.
/// Green once that argument is `strict_group`.
#[test]
fn noise_connection_affinity_overlay_is_refused_outside_a_strict_group() {
    let manager = live_manager(config_with(
        vec![
            account("at-inside", &["shared"]),
            account("at-outside", &[]),
        ],
        None,
    ));
    let now = OffsetDateTime::now_utc();
    let now_ms = odt_to_ms(now);

    // The control FIRST, on its own connection key: with no group asked
    // for, a request follows its connection's already-affined account.
    // Checked before the strict ask below on a SEPARATE key, because a
    // strict ask that falls through to the normal pick re-records
    // `conn_affinity` for whichever key it used, sharing one key would
    // make the control assertion read the strict call's own side effect
    // instead of the overlay's un-narrowed behaviour.
    let control_conn_key = 100_u64;
    manager.conn_affinity_record(control_conn_key, 1, now_ms);
    assert_eq!(
        manager.select(
            &HashSet::new(),
            now,
            None,
            None,
            "/mcp-registry/v1/list",
            Some(control_conn_key),
        ),
        Some(1),
        "with no group asked for, connection affinity follows the account \
         an earlier request on this connection left it on"
    );

    // THE SUBJECT: the same connection-affinity fact, but the request now
    // asks strictly for `shared`, which `at-outside` does not carry.
    let strict_conn_key = 99_u64;
    manager.conn_affinity_record(strict_conn_key, 1, now_ms);
    let served = manager
        .select_with_group(
            &HashSet::new(),
            now,
            None,
            None, // unpinned, this overlay is also gated on `keep_pin.is_none()`
            "/mcp-registry/v1/list",
            Some(strict_conn_key),
            Some("shared"),
        )
        .expect("the strict group still has a servable member");
    assert_ne!(
        served, 1,
        "`at-outside` carries no `shared` group; a strict ask must not \
         keep following a connection's account out of the group"
    );
}
