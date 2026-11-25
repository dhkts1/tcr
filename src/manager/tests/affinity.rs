//! Session affinity: pins, diverts, holds and re-keys, and the prompt-cache TTL settings.

use super::*;

/// A hold that OUTLIVES the prompt cache — the only kind that is ACCOUNT-level
/// death and may re-key a session. Derived from the threshold rather than
/// written as a literal so that raising [`CACHE_WARM_HOLD_SECS`] can never
/// silently turn a re-key test into a divert test.
const LONG_HOLD_SECS: i64 = CACHE_WARM_HOLD_SECS + 60;

/// Session affinity, headline: a session key pins to ONE account. Six selects
/// with the SAME key over three equal-priority accounts all land on the same
/// idx — the exact inverse of `select_spreads_load_across_a_priority_tier`.
#[test]
fn affinity_pins_session_to_one_account() {
    let manager = manager_of(vec![account("a", 0), account("b", 0), account("c", 0)]);
    let now = OffsetDateTime::now_utc();
    let first = manager
        .select(&HashSet::new(), now, None, Some(7), "/v1/messages", None)
        .expect("an account is eligible");
    for _ in 0..5 {
        assert_eq!(
            manager.select(&HashSet::new(), now, None, Some(7), "/v1/messages", None),
            Some(first),
            "the same session key must stay pinned to one account"
        );
    }
}

/// Regression guard: the disabled path (`affinity = None`) is byte-unchanged
/// behaviour — six selects over three accounts still fan out evenly [2,2,2].
#[test]
fn affinity_none_still_spreads() {
    assert_eq!(
        six_sessionless_selects_over_one_tier_of_three(),
        [2, 2, 2],
        "the disabled affinity path must spread exactly like today"
    );
}

/// Migration: when a session's pinned account becomes ineligible (rate
/// limited long enough to outlive its prompt cache), the next same-key select
/// re-pins to a DIFFERENT eligible account and then sticks to that one.
#[test]
fn affinity_repins_when_pinned_account_ineligible() {
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    let now = OffsetDateTime::now_utc();
    let pinned = manager
        .select(&HashSet::new(), now, None, Some(42), "/v1/messages", None)
        .expect("an account is eligible");
    manager.mark_rate_limited(pinned, LONG_HOLD_SECS);
    let repinned = manager
        .select(&HashSet::new(), now, None, Some(42), "/v1/messages", None)
        .expect("the other account is eligible");
    assert_ne!(repinned, pinned, "must migrate off the ineligible pin");
    // And it sticks to the new pin.
    for _ in 0..3 {
        assert_eq!(
            manager.select(&HashSet::new(), now, None, Some(42), "/v1/messages", None),
            Some(repinned),
            "the re-pin must be durable"
        );
    }
}

/// Accounts `a` and `b` at one priority, paced to one request in flight each, and the
/// account session `key`'s first select at `now` pinned it to.
fn one_in_flight_pair_with_a_pin(key: u64, now: OffsetDateTime) -> (Arc<Manager>, usize) {
    let pacing = PacingConfig {
        max_in_flight_per_account: Some(1),
        min_spacing_ms: None,
    };
    let manager = build_manager(
        config_with_pacing(vec![account("a", 0), account("b", 0)], pacing),
        counting_refresher(),
    );
    let pinned = manager
        .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
        .expect("an account is eligible");
    (manager, pinned)
}

/// PACING gates DIVERT, they never RE-KEY: a pinned account that is merely paced
/// (at the in-flight cap, every hard gate clear) yields THIS request to another
/// account while the pin stays put, and the session returns to it the moment the
/// guards drop. Before the fix the fall-through re-pin at the bottom of `select`
/// rewrote the pin to the diverted account, permanently cold-starting that
/// session's per-account prompt cache.
///
/// Contrast `soft_gated_pin_is_served_not_diverted`, where the OTHER soft gate —
/// our own utilization threshold — does not even divert. The difference is what
/// each gate knows: `in_flight` is a fact we measure exactly and continuously
/// about our own concurrency, and spreading a burst is the whole reason the cap
/// exists; utilization is arithmetic over headers that go stale by minutes.
#[test]
fn soft_paced_pin_diverts_without_repinning() {
    let now = OffsetDateTime::now_utc();
    let (manager, pinned) = one_in_flight_pair_with_a_pin(77, now);
    // Saturate ONLY the pinned account: at cap=1 it is soft-paced while every
    // hard gate (disabled/error/hold/quota) stays clear.
    {
        let mut a = manager.accounts.write().expect("accounts lock poisoned");
        a[pinned].in_flight = 1;
    }
    let diverted = manager
        .select(&HashSet::new(), now, None, Some(77), "/v1/messages", None)
        .expect("the un-paced account serves this request");
    assert_ne!(
        diverted, pinned,
        "a soft-paced pin must yield THIS request to the cooler account"
    );
    assert_eq!(
        pin_of(&manager, 77),
        Some(pinned),
        "a PACING divert must not move the pin"
    );
    // The pacing guard clears → the session snaps back to its warm account.
    {
        let mut a = manager.accounts.write().expect("accounts lock poisoned");
        a[pinned].in_flight = 0;
    }
    for _ in 0..3 {
        assert_eq!(
            manager.select(&HashSet::new(), now, None, Some(77), "/v1/messages", None),
            Some(pinned),
            "once un-paced the session must return to its original account"
        );
    }
}

/// The last-touch stamp is still refreshed on a pacing divert, so a heavily
/// diverted session can never become the `AFFINITY_CAP` eviction victim (the
/// eviction sorts on exactly this field).
#[test]
fn soft_paced_divert_refreshes_pin_last_touch() {
    let now = OffsetDateTime::now_utc();
    let (manager, pinned) = one_in_flight_pair_with_a_pin(88, now);
    {
        let mut a = manager.accounts.write().expect("accounts lock poisoned");
        a[pinned].in_flight = 1;
    }
    // A later `now` is what the divert must stamp onto the surviving pin.
    let later = now + Duration::seconds(30);
    let later_ms = odt_to_ms(later);
    manager
        .select(&HashSet::new(), later, None, Some(88), "/v1/messages", None)
        .expect("the un-paced account serves this request");
    let pins = manager.affinity.lock().expect("affinity lock poisoned");
    assert_eq!(
        pins.get(&88).copied(),
        Some((pinned, later_ms)),
        "the divert must re-insert the OLD index with a FRESH last-touch"
    );
}

/// If the pinned account is in `tried` AND fails a HARD gate, affinity falls
/// through to a normal pick and re-pins to the new account. Being in `tried`
/// is on its own only a SOFT signal (see
/// `transient_tried_failure_keeps_the_pin`) — it takes a hard gate, here a hold
/// that outlives the prompt cache, to make the fall-through durable. A SHORT
/// hold would not: it diverts and keeps the pin (see
/// `short_hold_diverts_but_keeps_the_pin`), so the fall-through would serve the
/// other account while leaving the session pinned where it was.
#[test]
fn affinity_falls_through_when_pinned_in_tried_and_hard_gated() {
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    let now = OffsetDateTime::now_utc();
    let a = manager
        .select(&HashSet::new(), now, None, Some(9), "/v1/messages", None)
        .expect("an account is eligible");
    // The failover that put A in `tried` was durable: it armed a hold that
    // outlives A's prompt cache, so there is nothing left to come home to.
    manager.mark_rate_limited(a, LONG_HOLD_SECS);
    let tried: HashSet<usize> = std::iter::once(a).collect();
    let b = manager
        .select(&tried, now, None, Some(9), "/v1/messages", None)
        .expect("the untried account is eligible");
    assert_ne!(b, a, "must fall through the tried pin to the other account");
    // The pin updated to B: a fresh same-key select with nothing tried sticks to B.
    assert_eq!(
        manager.select(&HashSet::new(), now, None, Some(9), "/v1/messages", None),
        Some(b),
        "the pin must have migrated to the fallen-through account"
    );
}

// ---- only a HARD gate may re-key a session ---------------------------------

/// THE cache-loss fix: an account crossing the SOFT utilization threshold used to
/// dump every session pinned to it at once, because `eligible()` folds
/// `quota.is_near` into the same bool as the hard gates. Keeping the pin while
/// still diverting the request — the first half of the fix — bought nothing: the
/// diverted request paid the cold prefix anyway (measured live: a 44.4%
/// account-switch rate on SUCCESSFUL serves, with zero hard failures). The
/// threshold is our own arithmetic, and it routinely benches an account Anthropic
/// is answering 200s for, so it may not bench a warm pin at all: the pinned
/// account SERVES. With eleven of thirteen live accounts sitting at 98-100%, this
/// is the single largest remaining cause of per-account prompt-cache loss.
#[test]
fn soft_gated_pin_is_served_not_diverted() {
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    let now = OffsetDateTime::now_utc();
    let pinned = manager
        .select(&HashSet::new(), now, None, Some(1234), "/v1/messages", None)
        .expect("an account is eligible");
    // Over the soft threshold, but Anthropic still says `allowed_warning`:
    // every HARD gate is clear.
    set_over_threshold(&manager, pinned, 0.995, "allowed_warning");

    for _ in 0..3 {
        assert_eq!(
            manager.select(&HashSet::new(), now, None, Some(1234), "/v1/messages", None),
            Some(pinned),
            "an over-threshold pin that clears every HARD gate still serves its \
             own session — upstream, not our utilization arithmetic, is the oracle"
        );
    }
    assert_eq!(
        pin_of(&manager, 1234),
        Some(pinned),
        "crossing the SOFT utilization threshold must not re-key the session"
    );

    // The threshold is not dead — it still steers selection with no pin to
    // protect, so a session arriving cold lands on the cooler account.
    let unpinned = manager
        .select(&HashSet::new(), now, None, None, "/v1/messages", None)
        .expect("the under-threshold account is eligible");
    assert_ne!(
        unpinned, pinned,
        "the soft threshold still gates UNPINNED selection"
    );
}

/// APP-16 (measured 2026-09-11): an idle-cold pin on an over-threshold account
/// has nothing left to lose by moving — its prompt cache is already cold by the
/// crate's own [`CACHE_WARM_HOLD_SECS`] clock, so serving the pin only keeps a
/// dead-weight session off the accounts sitting idle. `select` must fall through
/// to the normal pick, which durably re-keys onto the least-loaded ELIGIBLE
/// account, exactly as a genuinely dead pin does.
#[test]
fn idle_cold_session_on_over_threshold_account_rekeys_to_an_eligible_account() {
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    let now = OffsetDateTime::now_utc();
    set_over_threshold(&manager, 0, 0.95, "allowed_warning");
    let key = 9001u64;
    seed_pin(
        &manager,
        key,
        0,
        odt_to_ms(now) - (CACHE_WARM_HOLD_SECS + 1) * 1000,
    );

    let served = manager
        .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
        .expect("the eligible account b serves this request");
    assert_eq!(
        served, 1,
        "an idle-cold pin on an over-threshold account must fall through to \
         the eligible account, not serve the dead-weight pin"
    );
    assert_eq!(
        pin_of(&manager, key),
        Some(1),
        "the session's pin must durably move to the account that served it"
    );
}

/// The complement, pinned so the change above cannot widen: a session touched
/// well inside [`CACHE_WARM_HOLD_SECS`] still has a warm cache to protect, so an
/// over-threshold pin is served exactly as `soft_gated_pin_is_served_not_diverted`
/// already established — this is today's behaviour, unchanged.
#[test]
fn warm_session_on_over_threshold_account_keeps_its_pin() {
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    let now = OffsetDateTime::now_utc();
    set_over_threshold(&manager, 0, 0.95, "allowed_warning");
    let key = 9002u64;
    seed_pin(&manager, key, 0, odt_to_ms(now) - 10_000);

    let served = manager
        .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
        .expect("the pinned account is still eligible on hard gates");
    assert_eq!(
        served, 0,
        "a warm (well within CACHE_WARM_HOLD_SECS) over-threshold pin still \
         serves its own session"
    );
    assert_eq!(
        pin_of(&manager, key),
        Some(0),
        "the pin must not move while the cache is still warm"
    );
}

/// Falling through must not turn a soft threshold into a hard 429: with no
/// eligible alternative (B disabled), the idle-cold pin still serves its own
/// over-threshold account — the same last-resort the fall-through pick already
/// gives a genuinely dead pin.
#[test]
fn idle_cold_session_stays_when_no_eligible_alternative() {
    let manager = manager_of(vec![
        account("a", 0),
        Account {
            disabled: Some(true),
            ..account("b", 0)
        },
    ]);
    let now = OffsetDateTime::now_utc();
    set_over_threshold(&manager, 0, 0.95, "allowed_warning");
    let key = 9003u64;
    seed_pin(
        &manager,
        key,
        0,
        odt_to_ms(now) - (CACHE_WARM_HOLD_SECS + 1) * 1000,
    );

    let served = manager.select(&HashSet::new(), now, None, Some(key), "/v1/messages", None);
    assert_eq!(
        served,
        Some(0),
        "with no eligible alternative, an idle-cold pin must still serve its \
         over-threshold account rather than 429 the request"
    );
}

/// The guard against over-correcting: a HARD gate still re-keys durably. A
/// rate-limit hold that outlives the prompt cache means the account is gone for
/// longer than the session's prefix survives, so the session moves and STAYS
/// moved. (A SHORTER hold is the SOFT case — see
/// `short_hold_diverts_but_keeps_the_pin`.)
#[test]
fn pin_with_live_hold_is_rekeyed() {
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    let now = OffsetDateTime::now_utc();
    let pinned = manager
        .select(&HashSet::new(), now, None, Some(2345), "/v1/messages", None)
        .expect("an account is eligible");
    manager.mark_rate_limited(pinned, LONG_HOLD_SECS);

    let served = manager
        .select(&HashSet::new(), now, None, Some(2345), "/v1/messages", None)
        .expect("the un-held account serves this request");
    assert_ne!(served, pinned, "a held pin cannot serve");
    assert_eq!(
        manager
            .affinity
            .lock()
            .expect("affinity lock poisoned")
            .get(&2345)
            .map(|&(idx, _)| idx),
        Some(served),
        "a hold outliving the prompt cache is a HARD gate — it must re-key the \
         session"
    );
}

/// THE guard against over-correcting serve-the-pin into serve-anything: an
/// account that is over the soft threshold AND holds a live 429 must still
/// rotate. This is the exact combination the serve-the-pin path could swallow —
/// the soft gate that now serves, stacked on the hard gate that must still win —
/// and it is also the self-healing loop that makes serving over the threshold
/// safe: a genuinely rejected account answers with a 429, that 429 arms
/// `rate_limited_until_ms`, and this select is the one that moves the session off
/// it. `hard_ok` stays the sole authority in both directions.
#[test]
fn pin_that_fails_a_hard_gate_still_rotates() {
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    let now = OffsetDateTime::now_utc();
    let pinned = manager
        .select(&HashSet::new(), now, None, Some(5678), "/v1/messages", None)
        .expect("an account is eligible");
    set_over_threshold(&manager, pinned, 0.995, "allowed_warning");
    // The serve-over-threshold hit a real 429, which armed a real hold — and a
    // long one, past the point where waiting it out could still hit a warm cache.
    manager.mark_rate_limited(pinned, LONG_HOLD_SECS);

    let served = manager
        .select(&HashSet::new(), now, None, Some(5678), "/v1/messages", None)
        .expect("the un-held account serves this request");
    assert_ne!(
        served, pinned,
        "a live hold is HARD — it outranks the serve-the-pin path"
    );
    assert_eq!(
        pin_of(&manager, 5678),
        Some(served),
        "and it re-keys the session DURABLY, not just for this request"
    );
    // Durable: a fresh select with nothing tried stays on the new account.
    assert_eq!(
        manager.select(&HashSet::new(), now, None, Some(5678), "/v1/messages", None),
        Some(served),
        "the session must not snap back to the held account"
    );
}

/// `quota.status == "rejected"` is Anthropic's own verdict, so it is HARD even
/// though the account looks identical to the `allowed_warning` case above in
/// every other field. This pair is the whole soft/hard distinction in two tests.
#[test]
fn pin_rejected_by_upstream_is_rekeyed() {
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    let now = OffsetDateTime::now_utc();
    let pinned = manager
        .select(&HashSet::new(), now, None, Some(3456), "/v1/messages", None)
        .expect("an account is eligible");
    set_over_threshold(&manager, pinned, 0.995, "rejected");

    let served = manager
        .select(&HashSet::new(), now, None, Some(3456), "/v1/messages", None)
        .expect("the allowed account serves this request");
    assert_ne!(served, pinned, "a rejected pin cannot serve");
    assert_eq!(
        manager
            .affinity
            .lock()
            .expect("affinity lock poisoned")
            .get(&3456)
            .map(|&(idx, _)| idx),
        Some(served),
        "an upstream `rejected` is a HARD gate — it must re-key the session"
    );
}

/// A pin landing in `tried` means this ONE request failed over it — a dropped
/// connection or a 5xx, not proof the account is gone. Divert, keep the pin.
/// Self-healing: when the failure is durable it arms a hold, and the very next
/// select re-keys on that evidence instead of on a single blip.
#[test]
fn transient_tried_failure_keeps_the_pin() {
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    let now = OffsetDateTime::now_utc();
    let pinned = manager
        .select(&HashSet::new(), now, None, Some(4567), "/v1/messages", None)
        .expect("an account is eligible");
    let tried: HashSet<usize> = std::iter::once(pinned).collect();

    let served = manager
        .select(&tried, now, None, Some(4567), "/v1/messages", None)
        .expect("the untried account serves this request");
    assert_ne!(served, pinned, "the tried pin cannot serve THIS request");
    assert_eq!(
        manager
            .affinity
            .lock()
            .expect("affinity lock poisoned")
            .get(&4567)
            .map(|&(idx, _)| idx),
        Some(pinned),
        "a transient failover must not re-key the session"
    );
    // Nothing tried → straight back to the warm pin.
    assert_eq!(
        manager.select(&HashSet::new(), now, None, Some(4567), "/v1/messages", None),
        Some(pinned),
        "the session must return to its original account"
    );

    // Now the failure proves durable (a 429 armed a hold long enough to outlive
    // the account's prompt cache) — that IS hard, so the same tried-pin select
    // re-keys.
    manager.mark_rate_limited(pinned, LONG_HOLD_SECS);
    let moved = manager
        .select(&tried, now, None, Some(4567), "/v1/messages", None)
        .expect("the un-held account serves this request");
    assert_eq!(
        manager
            .affinity
            .lock()
            .expect("affinity lock poisoned")
            .get(&4567)
            .map(|&(idx, _)| idx),
        Some(moved),
        "a durable failure still re-keys — the divert-and-keep is self-healing"
    );
}

/// Model-scoped exhaustion is a property of the REQUEST CLASS, not of the
/// account: an account out of its Fable weekly still answers Opus perfectly. So a
/// Fable request whose pin is Fable-exhausted diverts THAT ONE request and keeps
/// the pin — the session's next Opus turn must still land on the account holding
/// its warm prefix.
///
/// This is the live shape, not a corner case: every Claude Code session mixes
/// classes (Opus for the conversation, a one-line Fable call for titles and
/// summaries) while a real fleet reads 95-99% on the Fable weekly across nearly
/// every account. Counted as account death, one cheap title request re-keyed a
/// 200k-token conversation onto a cold account.
#[test]
fn fable_exhaustion_diverts_the_request_but_keeps_the_pin() {
    let manager = manager_of(vec![account("home", 0), account("other", 0)]);
    let now = OffsetDateTime::now_utc();
    let key = 909_090u64;
    let home = manager
        .select(
            &HashSet::new(),
            now,
            Some("claude-opus-4-6"),
            Some(key),
            "/v1/messages",
            None,
        )
        .expect("an account is eligible");
    set_fable_exhausted(&manager, home, 0.999);

    let served = manager
        .select(
            &HashSet::new(),
            now,
            Some("claude-fable-5"),
            Some(key),
            "/v1/messages",
            None,
        )
        .expect("the other account can still serve Fable");
    assert_ne!(
        served, home,
        "a Fable request must not be served from an exhausted Fable weekly"
    );
    assert_eq!(
        pin_of(&manager, key),
        Some(home),
        "model-scoped exhaustion is a per-REQUEST fact — it may divert a \
         request, it may never re-key the session"
    );
    assert_eq!(
        manager.select(
            &HashSet::new(),
            now,
            Some("claude-opus-4-6"),
            Some(key),
            "/v1/messages",
            None
        ),
        Some(home),
        "the next non-Fable request must come home to the warm prefix"
    );
}

/// THE over-correction guard for the test above: the model gate softened, and
/// nothing else did. Same Fable-exhausted pin, but now the account is also under a
/// 429 hold that outlives its prompt cache — ACCOUNT-level death, which must still
/// re-key durably.
#[test]
fn account_death_still_rekeys_a_fable_session() {
    let manager = manager_of(vec![account("home", 0), account("other", 0)]);
    let now = OffsetDateTime::now_utc();
    let key = 808_080u64;
    let home = manager
        .select(
            &HashSet::new(),
            now,
            Some("claude-opus-4-6"),
            Some(key),
            "/v1/messages",
            None,
        )
        .expect("an account is eligible");
    set_fable_exhausted(&manager, home, 0.999);
    // The account is not merely out of Fable — it is gone for every model class,
    // and for longer than its prompt cache survives.
    manager.mark_rate_limited(home, LONG_HOLD_SECS);

    let served = manager
        .select(
            &HashSet::new(),
            now,
            Some("claude-fable-5"),
            Some(key),
            "/v1/messages",
            None,
        )
        .expect("the un-held account serves this request");
    assert_ne!(served, home, "a held pin cannot serve");
    assert_eq!(
        pin_of(&manager, key),
        Some(served),
        "a hold outliving the prompt cache is ACCOUNT-level death — it must \
         still re-key the session"
    );
    assert_eq!(
        manager.select(
            &HashSet::new(),
            now,
            Some("claude-opus-4-6"),
            Some(key),
            "/v1/messages",
            None
        ),
        Some(served),
        "and durably: the Opus turn stays on the failover, it does not snap back"
    );
}

/// The same invariant one path over. When the whole fleet reads over the SOFT
/// threshold, `select` returns `None` and the request lands in
/// `select_revalidation` — which also re-pins. A Fable request whose pin is
/// Fable-exhausted must be SERVED elsewhere there too while the pin stays put.
///
/// Live-reachable, not theoretical: a fleet at 95-99% Fable has no Fable-eligible
/// account for `select` to pick, so this is the path a title request actually
/// takes.
#[test]
fn revalidation_fable_divert_keeps_the_pin() {
    let manager = manager_of(vec![account("home", 0), account("other", 0)]);
    let now = OffsetDateTime::now_utc();
    let key = 707_070u64;
    let home = manager
        .select(
            &HashSet::new(),
            now,
            Some("claude-opus-4-6"),
            Some(key),
            "/v1/messages",
            None,
        )
        .expect("an account is eligible");
    let other = 1 - home;
    // The whole fleet crosses the SOFT threshold, and the pin is out of Fable.
    set_over_threshold(&manager, home, 0.99, "allowed_warning");
    set_over_threshold(&manager, other, 0.96, "allowed_warning");
    set_fable_exhausted(&manager, home, 0.999);

    assert_eq!(
        manager.select(
            &HashSet::new(),
            now,
            Some("claude-fable-5"),
            Some(key),
            "/v1/messages",
            None
        ),
        None,
        "every account is over the soft threshold → normal select benches all"
    );
    assert_eq!(
        manager.select_revalidation(
            &HashSet::new(),
            now,
            Some("claude-fable-5"),
            Some(key),
            "/v1/messages"
        ),
        Some(other),
        "the revalidation serve routes the Fable request to an account that can \
         answer it"
    );
    assert_eq!(
        pin_of(&manager, key),
        Some(home),
        "and it must NOT have re-keyed the session — the pin is model-blocked \
         for this request only"
    );
    // A non-Fable request comes straight home: `select`'s pin-honor path serves
    // the over-threshold pin rather than diverting it.
    assert_eq!(
        manager.select(
            &HashSet::new(),
            now,
            Some("claude-opus-4-6"),
            Some(key),
            "/v1/messages",
            None
        ),
        Some(home),
        "the Opus turn is served by the warm pin"
    );
}

/// A rate-limit hold is a TIMER, not a death — and the timers this proxy arms
/// are mostly SHORT: a no-guidance transient 429 parks 15s + jitter, and a
/// `retry-after` park is clamped to 300s (`src/proxy.rs`). Anthropic's default
/// ephemeral prompt cache lives 5 minutes, so re-keying on a 15-second park
/// throws away a prefix that would still be warm 15 seconds later — and the
/// session never returns to it, because a re-key is durable.
///
/// So a hold that clears inside [`CACHE_WARM_HOLD_SECS`] is SOFT: it diverts
/// THIS one request (the account really is parked and would only answer another
/// 429) and leaves the pin alone, and the session comes home warm when the timer
/// runs out. The over-correction guard is `long_hold_rekeys_the_session`, which
/// is this same fixture with only the hold duration changed.
#[test]
fn short_hold_diverts_but_keeps_the_pin() {
    let manager = manager_of(vec![account("home", 0), account("other", 0)]);
    let now = OffsetDateTime::now_utc();
    let key = 515_151u64;
    let home = manager
        .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
        .expect("an account is eligible");
    manager.mark_rate_limited(home, SHORT_HOLD_SECS);

    let diverted = manager
        .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
        .expect("the un-held account serves this request");
    assert_ne!(
        diverted, home,
        "a parked pin cannot serve THIS request — serving it only buys a 429"
    );
    assert_eq!(
        pin_of(&manager, key),
        Some(home),
        "a hold that clears while the cache is still warm may divert a request, \
         it may never re-key the session"
    );

    // The timer runs out. A past hold reads as expired live, no mutation needed.
    let after = now + Duration::seconds(SHORT_HOLD_SECS + 5);
    for _ in 0..3 {
        assert_eq!(
            manager.select(
                &HashSet::new(),
                after,
                None,
                Some(key),
                "/v1/messages",
                None
            ),
            Some(home),
            "once the hold clears the session must come home to the account \
             still holding its warm prefix"
        );
    }
}

/// THE over-correction guard for the test above: only the SHORT holds softened.
/// The same fixture with a hold that outlives the prompt cache must still re-key
/// durably — by the time that account frees, the prefix it was holding is gone,
/// so keeping the pin would divert through the LRU pick on every turn and
/// scatter the conversation cold across the fleet instead of settling it on one
/// account that can warm up.
#[test]
fn long_hold_rekeys_the_session() {
    let manager = manager_of(vec![account("home", 0), account("other", 0)]);
    let now = OffsetDateTime::now_utc();
    let key = 525_252u64;
    let home = manager
        .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
        .expect("an account is eligible");
    manager.mark_rate_limited(home, LONG_HOLD_SECS);

    let served = manager
        .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
        .expect("the un-held account serves this request");
    assert_ne!(served, home, "a parked pin cannot serve");
    assert_eq!(
        pin_of(&manager, key),
        Some(served),
        "a hold that outlives the prompt cache is ACCOUNT-level death — it must \
         re-key the session"
    );
    // Durable: a fresh select with nothing tried stays on the new account.
    assert_eq!(
        manager.select(&HashSet::new(), now, None, Some(key), "/v1/messages", None),
        Some(served),
        "the session must not snap back to the held account"
    );
}

/// Read the divert episode currently recorded for a session key, if any.
fn divert_episode_of(manager: &Manager, key: u64) -> Option<DivertEpisode> {
    manager
        .divert_ledger
        .lock()
        .expect("divert ledger lock poisoned")
        .get(&key)
        .copied()
}

/// Phase 1's headline claim (the divert-budget design notes §4.3, Phase 1
/// test list): a SECOND divert inside the SAME hold episode reuses the
/// FIRST divert's destination, rather than the normal LRU spread's own
/// anti-churn steering it onto a *different* alternate. Three accounts
/// (not two) so this is a real assertion: with only one alternate
/// available, "the second divert lands on the same account" would be true
/// with or without stickiness.
#[test]
fn sticky_divert_reuses_first_destination() {
    let manager = manager_of(vec![
        account("home", 0),
        account("alt1", 0),
        account("alt2", 0),
    ]);
    let now = OffsetDateTime::now_utc();
    let key = 616_161u64;
    let home = manager
        .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
        .expect("an account is eligible");
    manager.mark_rate_limited(home, SHORT_HOLD_SECS);

    let first_dest = manager
        .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
        .expect("an un-held alternate serves this request");
    assert_ne!(first_dest, home, "the held pin cannot serve THIS request");

    // Without stickiness, the normal LRU/priority pick's own anti-churn
    // (`select()`'s doc-comment: "consecutive requests fan out instead of
    // hammering one account") would steer the SECOND divert onto the
    // account that was NOT just selected. With the sticky overlay it must
    // land back on `first_dest` instead.
    let second_dest = manager
        .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
        .expect("an un-held alternate serves this request");
    assert_eq!(
        second_dest, first_dest,
        "a second divert in the same hold episode must reuse the first \
         divert's destination, not spend a second fresh account"
    );
    assert_eq!(
        pin_of(&manager, key),
        Some(home),
        "sticky reuse is a divert, not a re-key — the pin stays home"
    );

    let episode = divert_episode_of(&manager, key).expect("episode recorded");
    assert_eq!(episode.pin, home);
    assert_eq!(episode.sticky, first_dest);
    assert_eq!(
        episode.destinations.count_ones(),
        1,
        "two diverts reusing the SAME destination is one distinct \
         destination, not two"
    );
}

/// Episode boundary (the divert-budget design notes §4.1: "Reset is
/// structural, not timed"): a session that diverts, recovers, and is then
/// held AGAIN on a later, distinct hold must start the second episode
/// with a clean mask — the earlier hold's destination must not leak in as
/// a stale sticky pick for a hold it never happened during.
#[test]
fn new_hold_deadline_resets_the_episode() {
    let manager = manager_of(vec![
        account("home", 0),
        account("alt1", 0),
        account("alt2", 0),
    ]);
    // A single wall-clock read for the whole test — both hold deadlines
    // below are EXPLICIT, fixed offsets from this one `now_ms`, never
    // from `mark_rate_limited` (which takes its own, separate real-clock
    // read via `crate::now_ms()`). Mixing two independent clock reads is
    // exactly what made an earlier version of this test flaky: two reads
    // microseconds apart are USUALLY distinguishable at millisecond
    // resolution, but "usually" is not a gate. Deterministic by
    // construction now, not by luck.
    let now = OffsetDateTime::now_utc();
    let now_ms = odt_to_ms(now);
    let key = 626_262u64;
    let home = manager
        .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
        .expect("an account is eligible");
    {
        let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
        accounts[home].rate_limited_until_ms = Some(now_ms + SHORT_HOLD_SECS * 1000);
    }
    manager
        .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
        .expect("an un-held alternate serves this request");
    let episode_one = divert_episode_of(&manager, key).expect("episode recorded");

    // Let the first hold clear and the session come home.
    let after = now + Duration::seconds(SHORT_HOLD_SECS + 5);
    assert_eq!(
        manager.select(
            &HashSet::new(),
            after,
            None,
            Some(key),
            "/v1/messages",
            None
        ),
        Some(home),
        "the hold cleared — the session returns to its warm pin"
    );

    // A SECOND, later hold on the SAME account: a different
    // `rate_limited_until_ms`, therefore a different episode identity.
    // Still derived from the SAME `now_ms` capture above, at a
    // deliberately distinct explicit offset — see
    // `DivertEpisode::until_ms`'s doc comment for why an ACCIDENTAL
    // collision between two real holds would be benign; this test simply
    // does not rely on one.
    {
        let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
        accounts[home].rate_limited_until_ms =
            Some(now_ms + (SHORT_HOLD_SECS + 5) * 1000 + SHORT_HOLD_SECS * 1000);
    }
    manager
        .select(
            &HashSet::new(),
            after,
            None,
            Some(key),
            "/v1/messages",
            None,
        )
        .expect("an un-held alternate serves this request");
    let episode_two = divert_episode_of(&manager, key).expect("episode recorded");

    assert_ne!(
        episode_one.until_ms, episode_two.until_ms,
        "two separate holds on the same account carry two different deadlines"
    );
    assert_eq!(
        episode_two.destinations.count_ones(),
        1,
        "the new episode starts with a FULL budget — a clean, single-bit \
         mask, not the first episode's carried-over distinct-destination count"
    );
}

/// Pin a session, park its account with EXACTLY `remaining_ms` left to run, and
/// report whether the next select RE-KEYED the session (`true`) or merely
/// diverted the request while keeping the pin (`false`).
///
/// Writes `rate_limited_until_ms` directly instead of going through
/// `mark_rate_limited`, which anchors the deadline to the process wall clock and
/// therefore cannot express an exact remaining duration relative to the `now`
/// the select is given — the whole point of a boundary test.
fn rekeys_with_hold_remaining(remaining_ms: i64) -> bool {
    rekeys_with_hold_remaining_for(remaining_ms, false)
}

/// [`rekeys_with_hold_remaining`] for a session whose latest request asked
/// for the 1h cache (`extended`), recorded the way the request path records
/// it, through `note_affinity_ttl`, before the hold is weighed.
fn rekeys_with_hold_remaining_for(remaining_ms: i64, extended: bool) -> bool {
    let manager = manager_of(vec![account("home", 0), account("other", 0)]);
    let now = OffsetDateTime::now_utc();
    let now_ms = odt_to_ms(now);
    let key = 424_242u64;
    manager.note_affinity_ttl(key, extended);
    let home = manager
        .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
        .expect("an account is eligible");
    {
        let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
        accounts[home].status = AccountStatus::Throttled;
        accounts[home].rate_limited_until_ms = Some(now_ms + remaining_ms);
    }
    let served = manager
        .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
        .expect("the un-held account serves this request");
    assert_ne!(
        served, home,
        "a parked pin cannot serve, at any hold length"
    );
    pin_of(&manager, key) != Some(home)
}

/// Pins the dividing line itself, from BOTH sides, so a refactor cannot drift it
/// silently: a hold with exactly [`CACHE_WARM_HOLD_SECS`] left is LONG (the cache
/// dies at the same instant the account frees, so there is nothing to come home
/// to), and one millisecond less is SHORT.
///
/// Asserted through `select`, not against the predicate, so it is the ROUTING
/// decision that is nailed down rather than an implementation detail.
#[test]
fn hold_exactly_at_the_boundary_is_treated_as_long() {
    let boundary_ms = CACHE_WARM_HOLD_SECS * 1000;
    assert!(
        rekeys_with_hold_remaining(boundary_ms),
        "a hold with exactly CACHE_WARM_HOLD_SECS left must re-key — the prefix \
         is gone at the same instant the account frees"
    );
    assert!(
        !rekeys_with_hold_remaining(boundary_ms - 1),
        "one millisecond under the line must keep the pin — the boundary is \
         `>=`, and this is the assertion that stops it drifting"
    );
}

/// A session whose latest request asked for the 1h cache is weighed against
/// its own window ([`WarmWindow::EXTENDED`], 55 minutes), not the 5-minute
/// default: a 20-minute hold re-keys a default session and only diverts a 1h
/// one, whose prefix is still warm when the account frees. The boundary is
/// pinned from both sides the same way the default one is above.
///
/// Watched red on the tree before `warm_window`, where every session got the
/// default: the second assertion failed, the 1h session re-keyed.
#[test]
fn an_extended_ttl_session_keeps_its_pin_through_a_hold_the_default_rekeys_on() {
    let twenty_minutes_ms = 20 * 60 * 1000;
    assert!(
        rekeys_with_hold_remaining_for(twenty_minutes_ms, false),
        "a default session re-keys: 20 minutes outlives a 5-minute cache"
    );
    assert!(
        !rekeys_with_hold_remaining_for(twenty_minutes_ms, true),
        "a 1h session keeps its pin: 20 minutes clears while its cache is warm"
    );
    let boundary_ms = WarmWindow::EXTENDED.ms();
    assert!(
        rekeys_with_hold_remaining_for(boundary_ms, true),
        "a hold exactly as long as the extended window re-keys even a 1h session"
    );
    assert!(
        !rekeys_with_hold_remaining_for(boundary_ms - 1, true),
        "one millisecond under the extended line keeps the pin"
    );
}

/// [`rekeys_with_hold_remaining_for`] for the other timer: the pinned account
/// carries upstream's `rejected` verdict on its 5h window, with that window's
/// reset `remaining_ms` away. No `rate_limited_until_ms` is armed, so the only
/// clock the router can read is the quota's own reset.
fn rekeys_with_rejection_rolling_in(remaining_ms: i64, extended: bool) -> bool {
    let manager = manager_of(vec![account("home", 0), account("other", 0)]);
    let now = OffsetDateTime::now_utc();
    let key = 535_353u64;
    manager.note_affinity_ttl(key, extended);
    let home = manager
        .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
        .expect("an account is eligible");
    {
        let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
        accounts[home].quota.status = Some("rejected".to_string());
        accounts[home].quota.five_hour = Some(crate::records::quota::QuotaWindow {
            utilization: 1.0,
            reset: Some(now + Duration::milliseconds(remaining_ms)),
        });
    }
    let served = manager
        .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
        .expect("the un-rejected account serves this request");
    assert_ne!(
        served, home,
        "a rejected account never serves, whatever its reset"
    );
    pin_of(&manager, key) != Some(home)
}

/// A `rejected` 5h status is a timer, not a death, when its window rolls while
/// the session's cache is still warm: the pin survives and the request diverts,
/// under the session's own window (5 minutes by default, 55 for a 1h session).
/// One that rolls after the cache is cold re-keys, as before.
///
/// Watched red on the tree before `rejection_clears_while_warm`, where
/// `rejected` was terminal under every window: the first and third assertions
/// failed, the session re-keyed.
#[test]
fn a_5h_rejection_rolling_inside_the_warm_window_keeps_the_pin() {
    let four_minutes_ms = 4 * 60 * 1000;
    let twenty_minutes_ms = 20 * 60 * 1000;
    assert!(
        !rekeys_with_rejection_rolling_in(four_minutes_ms, false),
        "default session: a rejection rolling in 4 minutes keeps the pin"
    );
    assert!(
        rekeys_with_rejection_rolling_in(twenty_minutes_ms, false),
        "default session: one rolling in 20 minutes outlives the cache and re-keys"
    );
    assert!(
        !rekeys_with_rejection_rolling_in(twenty_minutes_ms, true),
        "1h session: 20 minutes is inside its 55-minute window, pin kept"
    );
    assert!(
        rekeys_with_rejection_rolling_in(WarmWindow::EXTENDED.ms(), true),
        "1h session: a reset exactly at the window's edge re-keys"
    );
}

/// The 1h rewrite's default follows the fleet: on when every account is
/// OAuth (the window counts a 1h write like a 5m one), off as soon as one
/// row of an unrecognized type is present (a static key that may be billed
/// 2x against 1.25x on every write), and an explicit `cacheTtlRewrite` wins
/// either way. An API-key row cannot be the trigger: it never loads.
///
/// Watched red before the default read the account types: the second
/// assertion failed, a fleet with an API-key account still rewrote.
#[test]
fn cache_ttl_rewrite_defaults_to_the_fleets_account_types() {
    let all_oauth = manager_of(vec![account("a", 0), account("b", 0)]);
    assert!(all_oauth.cache_ttl_rewrite_enabled(), "all OAuth: on");

    let mut api_key = account("k", 0);
    api_key.account_type = config::AccountType::Unrecognized("static".to_string());
    let mixed = manager_of(vec![account("a", 0), api_key.clone()]);
    assert!(
        !mixed.cache_ttl_rewrite_enabled(),
        "one API-key account: off, its writes are billed"
    );

    let mut forced = config_with(vec![api_key]);
    forced
        .extra
        .insert("cacheTtlRewrite".to_string(), serde_json::Value::Bool(true));
    assert!(
        build_manager(forced, counting_refresher()).cache_ttl_rewrite_enabled(),
        "an explicit true overrides the default"
    );

    let mut off = config_with(vec![account("a", 0)]);
    off.extra.insert(
        "cacheTtlRewrite".to_string(),
        serde_json::Value::Bool(false),
    );
    assert!(
        !build_manager(off, counting_refresher()).cache_ttl_rewrite_enabled(),
        "an explicit false overrides the default"
    );
}

/// Once a hold LONG enough to have re-keyed a session has moved it onto a
/// failover account, the expiry of that hold does NOT bring the session home:
/// the pin is simply the failover from then on.
///
/// That is the whole justification for re-keying on a long hold at all. By the
/// time the original account frees, the prefix it was holding is gone
/// ([`CACHE_WARM_HOLD_SECS`]), so coming home would buy a cold start on the way
/// back and no cache hit at the other end — strictly worse than staying put.
///
/// The complement is `short_hold_diverts_but_keeps_the_pin`: a hold that clears
/// while the cache is still warm never re-keys in the first place, so there is
/// nothing to come home FROM. The two together are the whole rule.
#[test]
fn hold_expiry_leaves_the_session_on_its_failover() {
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    let now = OffsetDateTime::now_utc();
    let key = 606_060u64;
    let home = manager
        .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
        .expect("an account is eligible");
    manager.mark_rate_limited(home, LONG_HOLD_SECS);

    let failover = manager
        .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
        .expect("the un-held account serves this request");
    assert_ne!(
        failover, home,
        "a hold outliving the prompt cache re-keys the session"
    );
    assert_eq!(pin_of(&manager, key), Some(failover));

    // The session keeps working on its failover while the hold runs out, so its
    // cache there stays warm: one more turn a minute before the hold ends. A
    // session idle past its warm window would take the normal pick instead, and
    // that is `a_cold_pin_takes_the_reset_order_again`'s rule, not this one's.
    let last_turn = now + Duration::seconds(LONG_HOLD_SECS - 60);
    assert_eq!(
        manager.select(
            &HashSet::new(),
            last_turn,
            None,
            Some(key),
            "/v1/messages",
            None
        ),
        Some(failover),
        "while the hold is live the session is served on its failover"
    );

    // The hold expires. A past hold reads as expired live, no mutation needed.
    let after = now + Duration::seconds(LONG_HOLD_SECS + 40);
    assert_eq!(
        manager.select(
            &HashSet::new(),
            after,
            None,
            Some(key),
            "/v1/messages",
            None
        ),
        Some(failover),
        "the session stays on its failover once the hold clears — the original \
         account's prompt cache is long gone, so coming home would only buy a \
         second cold start"
    );
    assert_eq!(
        pin_of(&manager, key),
        Some(failover),
        "and the pin is not walked back either"
    );
}

/// Distinct session keys fan out to different accounts (each initial pin is a
/// normal LRU pick) and then each key stays on its own account.
#[test]
fn affinity_distinct_keys_fan_out_then_stick() {
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    let now = OffsetDateTime::now_utc();
    let x = manager
        .select(&HashSet::new(), now, None, Some(1), "/v1/messages", None)
        .expect("an account is eligible");
    let y = manager
        .select(&HashSet::new(), now, None, Some(2), "/v1/messages", None)
        .expect("an account is eligible");
    assert_ne!(x, y, "distinct keys' initial pins fan out across the tier");
    // Each key repeats onto its own account.
    for _ in 0..3 {
        assert_eq!(
            manager.select(&HashSet::new(), now, None, Some(1), "/v1/messages", None),
            Some(x)
        );
        assert_eq!(
            manager.select(&HashSet::new(), now, None, Some(2), "/v1/messages", None),
            Some(y)
        );
    }
}

/// The affinity map is bounded by `AFFINITY_CAP` + LRU-by-last-touch: filling it
/// past the cap evicts the single oldest-last-touch pin, and a recently-touched
/// pin survives even when it was the oldest by insertion order.
#[test]
fn affinity_map_is_bounded_by_cap_with_lru_eviction() {
    // Mirror of the private `AFFINITY_CAP` in `select`.
    const AFFINITY_CAP: usize = 1024;

    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    let base = OffsetDateTime::now_utc();

    // Fill to exactly CAP distinct keys, each stamped at a strictly later
    // last-touch than the previous (key i+1 touched at base + i seconds).
    for i in 0..AFFINITY_CAP {
        let key = (i + 1) as u64;
        manager
            .select(
                &HashSet::new(),
                base + Duration::seconds(i as i64),
                None,
                Some(key),
                "/v1/messages",
                None,
            )
            .expect("an account is eligible");
    }
    assert_eq!(
        manager
            .affinity
            .lock()
            .expect("affinity lock poisoned")
            .len(),
        AFFINITY_CAP,
        "map fills to exactly CAP without evicting yet (len == CAP, not > CAP)"
    );

    // Refresh the oldest-by-insertion pin (key 1) at the latest time, so its
    // last-touch becomes the newest and it must survive the next eviction. Key 2
    // is now the oldest-last-touch entry.
    manager.select(
        &HashSet::new(),
        base + Duration::seconds((AFFINITY_CAP + 10) as i64),
        None,
        Some(1),
        "/v1/messages",
        None,
    );

    // One more distinct key pushes len over CAP → evict the single oldest.
    manager.select(
        &HashSet::new(),
        base + Duration::seconds((AFFINITY_CAP + 11) as i64),
        None,
        Some((AFFINITY_CAP + 1) as u64),
        "/v1/messages",
        None,
    );

    let pins = manager.affinity.lock().expect("affinity lock poisoned");
    assert_eq!(pins.len(), AFFINITY_CAP, "eviction holds the map at CAP");
    assert!(pins.contains_key(&1), "the recently-touched pin survives");
    assert!(
        !pins.contains_key(&2),
        "the oldest-last-touch pin is evicted"
    );
    assert!(
        pins.contains_key(&((AFFINITY_CAP + 1) as u64)),
        "the just-inserted pin is present"
    );
}

/// The extended-cache set stays bounded with no pin flush to prune it: with affinity off
/// (no file, no flusher) every keyed request that asked for the 1h cache used to add a
/// key for good. Over the cap the least recently noted key goes, and the newest stays.
#[test]
fn the_extended_cache_set_is_capped_without_a_pin_flush() {
    let manager = manager_of(vec![account("a", 0)]);
    let noted = select::AFFINITY_CAP as u64 * 2;
    for key in 0..noted {
        manager.note_affinity_ttl(key, true);
    }
    let held = manager
        .affinity_extended
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .len();
    assert_eq!(held, select::AFFINITY_CAP);
    assert_eq!(manager.warm_window(Some(noted - 1)), WarmWindow::EXTENDED);
    assert_eq!(manager.warm_window(Some(0)), WarmWindow::DEFAULT);
}
