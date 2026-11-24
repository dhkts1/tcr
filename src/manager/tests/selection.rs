//! Account selection: priority tiers, eligibility, Fable bucket exhaustion, and revalidation when every account is over threshold.

use super::*;

#[test]
fn select_prefers_lower_priority_value() {
    let manager = manager_of(vec![account("low-pri", 5), account("high-pri", 0)]);
    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
        Some(1)
    );
}

#[test]
fn select_skips_disabled_account() {
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    manager.set_disabled(0, true);
    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
        Some(1)
    );
}

#[test]
fn select_skips_rate_limited_account() {
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    manager.mark_rate_limited(0, 300);
    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
        Some(1)
    );
}

#[test]
fn select_skips_account_over_threshold() {
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    let now = OffsetDateTime::now_utc();
    // Drive account 0 over the 0.90 threshold via real headers.
    let reset = (now + Duration::hours(1)).unix_timestamp();
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        "anthropic-ratelimit-unified-5h-utilization",
        "0.95".parse().unwrap(),
    );
    headers.insert(
        "anthropic-ratelimit-unified-5h-reset",
        reset.to_string().parse().unwrap(),
    );
    manager.update_quota(0, &headers, 200);
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
        Some(1)
    );
}

#[test]
fn select_returns_none_when_all_tried() {
    let manager = manager_of(vec![account("a", 0)]);
    let now = OffsetDateTime::now_utc();
    let tried: HashSet<usize> = std::iter::once(0).collect();
    assert_eq!(
        manager.select(&tried, now, None, None, "/v1/messages", None),
        None
    );
}

/// Load-balancing: within a priority tier, consecutive selects fan out across
/// every eligible account (least-recently-selected first) instead of pinning
/// the same one. Six sequential picks over three equal-priority accounts must
/// land exactly twice on each. A FIXED `now` (no clock advance) proves the
/// spread is driven by the monotonic select tick, not the wall clock — so a
/// same-millisecond burst still rotates.
#[test]
fn select_spreads_load_across_a_priority_tier() {
    assert_eq!(
        six_sessionless_selects_over_one_tier_of_three(),
        [2, 2, 2],
        "load must spread evenly across the tier, not pin one account"
    );
}

/// Priority still dominates the spread: a lower-priority-value account is
/// picked every time while it is eligible, even as its select tick advances —
/// load-balancing is *within* a tier, never across tiers.
#[test]
fn select_spread_respects_priority_tiers() {
    let manager = manager_of(vec![account("primary", 0), account("pillow", 10)]);
    let now = OffsetDateTime::now_utc();
    for _ in 0..5 {
        assert_eq!(
            manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
            Some(0),
            "the primary tier is never abandoned for a higher-priority-value account"
        );
    }
}

/// Drive an account's model-scoped weekly (`7d_oi`, Fable) bucket over
/// threshold via the real rate-limit headers the proxy learns from.
fn exhaust_oi_bucket(manager: &Manager, idx: usize, now: OffsetDateTime) {
    let reset = (now + Duration::hours(2)).unix_timestamp();
    let mut h = reqwest::header::HeaderMap::new();
    h.insert(
        "anthropic-ratelimit-unified-7d_oi-utilization",
        "0.95".parse().unwrap(),
    );
    h.insert(
        "anthropic-ratelimit-unified-7d_oi-reset",
        reset.to_string().parse().unwrap(),
    );
    manager.update_quota(idx, &h, 200);
}

/// Accounts `a` and `b` at one priority with `a`'s Fable bucket exhausted, and the
/// `now` it was exhausted at.
fn a_and_b_with_a_oi_exhausted() -> (Arc<Manager>, OffsetDateTime) {
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    let now = OffsetDateTime::now_utc();
    exhaust_oi_bucket(&manager, 0, now);
    (manager, now)
}

/// HEADLINE per-model routing: a Fable request SKIPS an account whose `7d_oi`
/// (Fable weekly) bucket is exhausted, and rotates to the next eligible one.
#[test]
fn fable_request_skips_oi_exhausted_account() {
    let (manager, now) = a_and_b_with_a_oi_exhausted();
    assert_eq!(
        manager.select(
            &HashSet::new(),
            now,
            Some("claude-fable-5"),
            None,
            "/v1/messages",
            None
        ),
        Some(1),
        "a Fable request must skip the OI-exhausted account 0"
    );
}

/// The SAME OI-exhausted account still serves every non-Fable model: with
/// account 1 already tried, a non-Fable request lands right back on account 0.
#[test]
fn fable_exhausted_account_still_serves_non_fable() {
    let (manager, now) = a_and_b_with_a_oi_exhausted();
    let tried: HashSet<usize> = std::iter::once(1).collect();
    assert_eq!(
        manager.select(
            &tried,
            now,
            Some("claude-opus-4-6"),
            None,
            "/v1/messages",
            None
        ),
        Some(0),
        "a non-Fable model must still serve from the OI-exhausted account 0"
    );
}

/// A request with no known model never consults the `7d_oi` bucket: account 0
/// (OI-exhausted) is still picked when it is the only untried account.
#[test]
fn no_model_ignores_oi_bucket() {
    let (manager, now) = a_and_b_with_a_oi_exhausted();
    let tried: HashSet<usize> = std::iter::once(1).collect();
    assert_eq!(
        manager.select(&tried, now, None, None, "/v1/messages", None),
        Some(0),
        "no-model traffic must ignore the model-scoped OI bucket"
    );
}

/// #1 Whole fleet over the soft threshold, all `allowed_warning`: normal
/// `select` is `None` but `select_revalidation` serves the LOWEST-utilization
/// account.
#[test]
fn revalidation_serves_least_utilized_when_all_over_threshold() {
    let manager = manager_of(vec![account("a", 0), account("b", 0), account("c", 0)]);
    set_over_threshold(&manager, 0, 0.995, "allowed_warning");
    set_over_threshold(&manager, 1, 0.970, "allowed_warning"); // least utilized
    set_over_threshold(&manager, 2, 0.999, "allowed_warning");

    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
        None,
        "every account is over the soft threshold → normal select benches all"
    );
    assert_eq!(
        manager.select_revalidation(&HashSet::new(), now, None, None, "/v1/messages"),
        Some(1),
        "revalidation serves the least-utilized allowed account"
    );
}

/// #1b THE cache-warmth test: a session pinned (affinity) to an over-threshold
/// but `allowed_warning` account X — with OTHER accounts LESS utilized — must be
/// served X (its warm pin), NOT the least-utilized other, and must NOT consume
/// the fallback anti-storm window. If X becomes `rejected`, it falls back to the
/// least-utilized survivor and RE-PINS the session there.
#[test]
fn revalidation_honors_pin_through_threshold() {
    let manager = manager_of(vec![account("a", 0), account("b", 0), account("pinned", 0)]);
    // The pinned account (idx 2) is the MOST utilized; the others are cooler.
    set_over_threshold(&manager, 0, 0.950, "allowed_warning"); // least utilized
    set_over_threshold(&manager, 1, 0.970, "allowed_warning");
    set_over_threshold(&manager, 2, 0.999, "allowed_warning"); // the warm pin
    let key = 424_242u64;
    let now = OffsetDateTime::now_utc();
    seed_pin(&manager, key, 2, odt_to_ms(now));

    assert_eq!(
        manager.select_revalidation(&HashSet::new(), now, None, Some(key), "/v1/messages"),
        Some(2),
        "pin-honor serves the session's warm pinned account, NOT the least-utilized other"
    );
    // The pin-honor path must NOT have burned the fallback throttle window: an
    // unaffiliated (no-pin) fallback serve still succeeds immediately.
    assert_eq!(
        manager.select_revalidation(&HashSet::new(), now, None, None, "/v1/messages"),
        Some(0),
        "pin-honor does not consume the fallback anti-storm window"
    );

    // When the pin becomes HARD-blocked (rejected), fall back to least-utilized
    // and RE-PIN the session there. Use a fresh manager (throttle valve reset).
    let manager2 = manager_of(vec![account("a", 0), account("b", 0), account("pinned", 0)]);
    set_over_threshold(&manager2, 0, 0.950, "allowed_warning"); // least utilized
    set_over_threshold(&manager2, 1, 0.970, "allowed_warning");
    set_over_threshold(&manager2, 2, 0.999, "rejected"); // pin now blocked
    seed_pin(&manager2, key, 2, odt_to_ms(now));
    assert_eq!(
        manager2.select_revalidation(&HashSet::new(), now, None, Some(key), "/v1/messages"),
        Some(0),
        "a rejected pin falls back to the least-utilized survivor"
    );
    let repinned = pin_of(&manager2, key);
    assert_eq!(
        repinned,
        Some(0),
        "the fallback serve re-pins the session to the account it chose"
    );
}

/// #1c `tried` governs whether an account may SERVE this request; it must never
/// decide whether that session's PIN may MOVE. A pin that merely failed THIS
/// request — a transport blip, a 5xx — while still clearing every ACCOUNT-level
/// HARD gate stays put: the fallback serves elsewhere for this one request and
/// the session comes home on the next.
///
/// Before the fix the `account_hard_ok → keep_pin` test sat INSIDE
/// `if !tried.contains(&idx)`, so a tried pin left `keep_pin` at `None` and the
/// fallback's `pins.insert` durably re-keyed the session — a warm prompt cache
/// discarded on the strength of one failed request. `select()` takes the opposite
/// decision on the identical fact and documents why; this is the two agreeing.
///
/// Also the first coverage of a NON-EMPTY `tried` on this path at all — every
/// other `select_revalidation` test passes `&HashSet::new()`.
#[test]
fn revalidation_keeps_the_pin_when_it_is_merely_tried() {
    let manager = manager_of(vec![account("a", 0), account("b", 0), account("pinned", 0)]);
    set_over_threshold(&manager, 0, 0.950, "allowed_warning"); // least utilized
    set_over_threshold(&manager, 1, 0.970, "allowed_warning");
    set_over_threshold(&manager, 2, 0.999, "allowed_warning"); // the warm pin
    let key = 515_151u64;
    let now = OffsetDateTime::now_utc();
    seed_pin(&manager, key, 2, odt_to_ms(now));

    // The pin failed THIS request and nothing more: not disabled, not errored,
    // not rejected, no hold. It clears every ACCOUNT-level hard gate.
    let tried = HashSet::from([2usize]);
    assert_eq!(
        manager.select_revalidation(&tried, now, None, Some(key), "/v1/messages"),
        Some(0),
        "a tried pin cannot serve THIS request → the least-utilized survivor does"
    );
    let pin_after = pin_of(&manager, key);
    assert_eq!(
        pin_after,
        Some(2),
        "the pin must NOT move: membership in `tried` is a per-request fact, not \
         evidence the account is gone"
    );
}

/// #1d The over-correction guard for #1c. Hoisting the `keep_pin` test out of the
/// `!tried` guard must not make a pin STICKY: a pin that fails an ACCOUNT-level
/// HARD gate still re-keys, and being in `tried` as well changes nothing. Both
/// hard shapes are exercised — `quota.status == "rejected"`, and a hold that
/// outlives the prompt cache (>= `CACHE_WARM_HOLD_SECS`).
#[test]
fn revalidation_rekeys_when_the_pin_fails_a_hard_gate() {
    let key = 626_262u64;
    let now = OffsetDateTime::now_utc();
    let tried = HashSet::from([2usize]);
    let pin_session = |manager: &Manager, idx: usize| seed_pin(manager, key, idx, odt_to_ms(now));
    let pin_of = |manager: &Manager| pin_of(manager, key);

    // (a) REJECTED and tried — a durable block, so the session must move off it.
    let rejected = manager_of(vec![account("a", 0), account("b", 0), account("pinned", 0)]);
    set_over_threshold(&rejected, 0, 0.950, "allowed_warning"); // least utilized
    set_over_threshold(&rejected, 1, 0.970, "allowed_warning");
    set_over_threshold(&rejected, 2, 0.999, "rejected");
    pin_session(&rejected, 2);
    assert_eq!(
        rejected.select_revalidation(&tried, now, None, Some(key), "/v1/messages"),
        Some(0),
        "a rejected pin is skipped; the least-utilized survivor serves"
    );
    assert_eq!(
        pin_of(&rejected),
        Some(0),
        "a rejected pin RE-KEYS — `tried` must not shield it from the hard gate"
    );

    // (b) HELD PAST THE CACHE and tried — also durable (a fresh manager, so the
    // anti-storm valve the serve above armed does not swallow this call).
    let held = manager_of(vec![account("a", 0), account("b", 0), account("pinned", 0)]);
    set_over_threshold(&held, 0, 0.950, "allowed_warning"); // least utilized
    set_over_threshold(&held, 1, 0.970, "allowed_warning");
    set_over_threshold(&held, 2, 0.999, "allowed_warning");
    {
        let mut a = held.accounts.write().expect("accounts lock poisoned");
        a[2].rate_limited_until_ms = Some(crate::now_ms() + 3_600_000);
    }
    pin_session(&held, 2);
    assert_eq!(
        held.select_revalidation(&tried, now, None, Some(key), "/v1/messages"),
        Some(0),
        "a pin held past its cache lifetime is skipped; the survivor serves"
    );
    assert_eq!(
        pin_of(&held),
        Some(0),
        "a hold that outlives the cache RE-KEYS — `tried` must not shield it"
    );
}

/// #2 A `rejected` account is skipped even when least-utilized; a higher-util
/// `allowed` one is served. If ALL are `rejected` → `None`.
#[test]
fn revalidation_skips_rejected_accounts() {
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    set_over_threshold(&manager, 0, 0.960, "rejected"); // least util but blocked
    set_over_threshold(&manager, 1, 0.990, "allowed_warning");

    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager.select_revalidation(&HashSet::new(), now, None, None, "/v1/messages"),
        Some(1),
        "the least-utilized account is rejected → skip it, serve the allowed one"
    );

    // Now reject BOTH → nothing servable → None.
    set_over_threshold(&manager, 1, 0.990, "rejected");
    // A fresh manager to reset the anti-storm valve (previous serve armed it).
    let manager2 = manager_of(vec![account("a", 0), account("b", 0)]);
    set_over_threshold(&manager2, 0, 0.960, "rejected");
    set_over_threshold(&manager2, 1, 0.990, "rejected");
    assert_eq!(
        manager2.select_revalidation(&HashSet::new(), now, None, None, "/v1/messages"),
        None,
        "all accounts rejected → no revalidation target, honest 429"
    );
}

/// #3 An account under a live rate-limit hold is skipped even if least-utilized.
#[test]
fn revalidation_skips_hard_held_account() {
    let manager = manager_of(vec![account("held", 0), account("free", 0)]);
    set_over_threshold(&manager, 0, 0.950, "allowed_warning"); // least util …
    set_over_threshold(&manager, 1, 0.990, "allowed_warning");
    {
        let mut a = manager.accounts.write().expect("accounts lock poisoned");
        a[0].rate_limited_until_ms = Some(crate::now_ms() + 60_000); // … but held.
    }
    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager.select_revalidation(&HashSet::new(), now, None, None, "/v1/messages"),
        Some(1),
        "a live-held account is a HARD block — skipped even as least-utilized"
    );
}

/// #4 Anti-storm: two back-to-back calls — the first serves, the second (inside
/// `REVALIDATION_MIN_SPACING_MS`) returns `None`.
#[test]
fn revalidation_throttle_spacing_respected() {
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    set_over_threshold(&manager, 0, 0.970, "allowed_warning");
    set_over_threshold(&manager, 1, 0.990, "allowed_warning");
    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager.select_revalidation(&HashSet::new(), now, None, None, "/v1/messages"),
        Some(0),
        "first call serves"
    );
    assert_eq!(
        manager.select_revalidation(&HashSet::new(), now, None, None, "/v1/messages"),
        None,
        "a second call within the spacing window is throttled to None"
    );
}

/// #5 When one account is UNDER threshold, normal `select` returns it and the
/// revalidation path is never consulted — the normal path stays unchanged.
#[test]
fn revalidation_not_consulted_when_an_account_is_eligible() {
    let manager = manager_of(vec![account("hot", 0), account("cool", 0)]);
    set_over_threshold(&manager, 0, 0.999, "allowed_warning");
    // account 1 stays healthy (no quota set) → under threshold → eligible.
    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
        Some(1),
        "an under-threshold account is served by the normal path; revalidation \
         is only consulted after select() returns None"
    );
}

/// #6 A Fable request skips an account whose Fable weekly (`7d_oi`) is a hard
/// reject, but a non-Fable request serves that same account.
#[test]
fn revalidation_fable_skips_fable_exhausted() {
    let build = || {
        let manager = manager_of(vec![account("fable-full", 0), account("other", 0)]);
        let now = OffsetDateTime::now_utc();
        // account 0: least-utilized on shared dims but Fable weekly exhausted.
        set_over_threshold(&manager, 0, 0.950, "allowed_warning");
        set_over_threshold(&manager, 1, 0.990, "allowed_warning");
        set_fable_exhausted(&manager, 0, 0.999);
        (manager, now)
    };

    // Fable request: account 0's Fable weekly is a hard reject → skip → serve 1.
    let (manager, now) = build();
    assert_eq!(
        manager.select_revalidation(
            &HashSet::new(),
            now,
            Some("claude-fable-5"),
            None,
            "/v1/messages"
        ),
        Some(1),
        "a Fable request skips the Fable-exhausted account even when least-utilized"
    );

    // Non-Fable request on a fresh manager: account 0 IS served (Fable weekly
    // gates Fable traffic only; on shared dims account 0 is least-utilized).
    let (manager, now) = build();
    assert_eq!(
        manager.select_revalidation(
            &HashSet::new(),
            now,
            Some("claude-opus-4-6"),
            None,
            "/v1/messages"
        ),
        Some(0),
        "a non-Fable request still serves the Fable-exhausted account"
    );
}
