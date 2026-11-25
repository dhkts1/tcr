//! In-flight caps, throttle slots, pacing, and the lifetime of the upstream serving client.

use super::*;

#[test]
fn enter_in_flight_stamps_the_wall_clock() {
    let manager = Manager::new(
        Config::fixture(vec![Account::fixture("alpha")]),
        Arc::new(oauth::NoRefresh),
        Arc::new(LiveUsageProber::new()),
        Arc::new(LiveWarmer::new()),
        None,
    );
    let before = crate::now_ms();
    manager.accounts.write().unwrap()[0].last_served_ms = before + 86_400_000;
    drop(manager.enter_in_flight(0));
    let after = crate::now_ms();
    let stamp = manager.accounts.read().unwrap()[0].last_served_ms;
    assert!(
        (before..=after).contains(&stamp),
        "enter_in_flight must stamp the wall clock directly: {before} <= {stamp} <= {after}"
    );
}

/// `build_serving_client` must not panic on either branch of `http1_only` —
/// h1 support is part of the reqwest/rustls TLS backend this crate already
/// pulls in, but a builder method rejecting a combination it doesn't support
/// is exactly the kind of thing that only shows up at `.build()`.
///
/// This cannot assert the built client actually NEGOTIATES h1 on the wire:
/// reqwest exposes no introspection on a built `Client` for its ALPN/HTTP
/// version policy, and asserting anything less than that would only prove
/// the bool reached this function's own argument — not that it reaches
/// reqwest's TLS layer. See the PR report for the explicit statement of
/// that gap.
#[test]
fn build_serving_client_builds_under_both_http1_only_settings() {
    let _off = build_serving_client(false);
    let _on = build_serving_client(true);
}

#[test]
fn throttle_slot_burst1_is_strict_spacing() {
    // B=1 (tau=0): threading the TAT across 3 calls at a fixed `now` yields
    // allow_at points spaced by exactly `spacing_ms`.
    let now = 1000;
    let spacing = 100;
    let (tat1, allow1) = throttle_slot(0, now, spacing, 1);
    assert_eq!(allow1, now); // first send: instant (allow_at == now)
    let (tat2, allow2) = throttle_slot(tat1, now, spacing, 1);
    assert_eq!(allow2, allow1 + spacing);
    let (_tat3, allow3) = throttle_slot(tat2, now, spacing, 1);
    assert_eq!(allow3, allow2 + spacing);
}

#[test]
fn throttle_slot_burst3_admits_then_paces() {
    // B=3, now=1000, T=100 (tau=200): first 3 fire instantly (allow_at <= now),
    // the 4th is paced to now + spacing_ms.
    let now = 1000;
    let spacing = 100;
    let burst = 3;
    let (tat1, allow1) = throttle_slot(0, now, spacing, burst);
    assert_eq!((tat1, allow1), (1100, 800));
    assert!(allow1 <= now);
    let (tat2, allow2) = throttle_slot(tat1, now, spacing, burst);
    assert_eq!((tat2, allow2), (1200, 900));
    assert!(allow2 <= now);
    let (tat3, allow3) = throttle_slot(tat2, now, spacing, burst);
    assert_eq!((tat3, allow3), (1300, 1000));
    assert!(allow3 <= now);
    let (tat4, allow4) = throttle_slot(tat3, now, spacing, burst);
    assert_eq!((tat4, allow4), (1400, 1100));
    assert_eq!(allow4, now + spacing); // 4th is paced
}

// ---- per-account request pacing (default-OFF; soft; RAII in-flight) --------

/// #1 Default-OFF is inert: an account that WOULD be paced (saturated
/// `in_flight` + just served) is still selected exactly as before, so an
/// unconfigured proxy is byte-identical to the pre-pacing build.
#[test]
fn pacing_off_is_inert() {
    let manager = manager_of(vec![account("solo", 0)]);
    {
        let mut a = manager.accounts.write().expect("accounts lock poisoned");
        a[0].in_flight = 5; // would exceed any cap …
        a[0].last_served_ms = crate::now_ms(); // … and be inside any spacing window.
    }
    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
        Some(0),
        "with PacingConfig::default() the account is selected regardless of load"
    );
}

/// #2 An account at the in-flight cap is skipped and a second, un-capped
/// account is chosen instead.
#[test]
fn in_flight_cap_makes_account_ineligible() {
    let pacing = PacingConfig {
        max_in_flight_per_account: Some(1),
        min_spacing_ms: None,
    };
    let manager = build_manager(
        config_with_pacing(vec![account("busy", 0), account("free", 0)], pacing),
        counting_refresher(),
    );
    {
        let mut a = manager.accounts.write().expect("accounts lock poisoned");
        a[0].in_flight = 1; // at cap=1 → ineligible under pacing.
    }
    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
        Some(1),
        "the capped account is skipped and the free one is chosen"
    );
}

/// #3 An account selected less than `min_spacing_ms` ago is skipped; once the
/// window elapses it is eligible again. Tested at `eligible()` so the soft
/// `select()` fallback can't mask the gate.
#[test]
fn min_spacing_skips_recently_served() {
    let pacing = PacingConfig {
        max_in_flight_per_account: None,
        min_spacing_ms: Some(1000),
    };
    let manager = build_manager(
        config_with_pacing(vec![account("a", 0)], pacing.clone()),
        counting_refresher(),
    );
    let now = OffsetDateTime::now_utc();
    let now_ms = odt_to_ms(now);
    {
        let mut a = manager.accounts.write().expect("accounts lock poisoned");
        a[0].last_served_ms = now_ms; // served 0ms ago.
    }
    let a = manager.accounts.read().expect("accounts lock poisoned");
    assert!(
        !Manager::eligible(
            &a[0],
            manager.global_threshold,
            manager.fable_weekly_threshold,
            &pacing,
            true,
            now,
            now_ms,
            false,
            crate::base::provider::Provider::Anthropic,
            None,
            None,
            &HashSet::new(),
            &HashSet::new(),
        ),
        "served 0ms ago (< 1000ms) → skipped"
    );
    let later_ms = now_ms + 1000;
    let later = ms_to_odt(later_ms).expect("valid timestamp");
    assert!(
        Manager::eligible(
            &a[0],
            manager.global_threshold,
            manager.fable_weekly_threshold,
            &pacing,
            true,
            later,
            later_ms,
            false,
            crate::base::provider::Provider::Anthropic,
            None,
            None,
            &HashSet::new(),
            &HashSet::new(),
        ),
        "after the spacing window → eligible again"
    );
}

/// #4 SOFT invariant: when EVERY account is paced out but otherwise healthy,
/// `select()` still returns Some (the least-loaded) — pacing never turns a
/// servable request into a failure.
#[test]
fn all_paced_falls_back_to_serve() {
    let pacing = PacingConfig {
        max_in_flight_per_account: Some(1),
        min_spacing_ms: None,
    };
    let manager = build_manager(
        config_with_pacing(vec![account("a", 0), account("b", 0)], pacing),
        counting_refresher(),
    );
    {
        let mut a = manager.accounts.write().expect("accounts lock poisoned");
        a[0].in_flight = 1;
        a[1].in_flight = 1; // both over cap=1.
    }
    let now = OffsetDateTime::now_utc();
    assert!(
        manager
            .select(&HashSet::new(), now, None, None, "/v1/messages", None)
            .is_some(),
        "all-paced but healthy → soft fallback serves least-loaded, never None"
    );
}

/// #5 The RAII in-flight guard decrements the count on Drop, so no serve-path
/// exit can leak a counter and strand an account.
#[test]
fn in_flight_guard_decrements_on_drop() {
    let manager = manager_of(vec![account("a", 0)]);
    {
        let _guard = manager.enter_in_flight(0);
        assert_eq!(
            manager.accounts.read().expect("accounts lock poisoned")[0].in_flight,
            1,
            "entering bumps the in-flight count"
        );
    }
    assert_eq!(
        manager.accounts.read().expect("accounts lock poisoned")[0].in_flight,
        0,
        "dropping the guard decrements back to 0"
    );
}

/// An account's upstream client is RETIRED once it has carried
/// [`MAX_SERVES_PER_CONNECTION`] sends, so its pooled h2 connection is
/// replaced before Anthropic's edge drains it (the reset distribution behind
/// that number is on the constant).
///
/// The control half matters as much as the assertion: one send short of the
/// budget the client must be the SAME `Arc`. Without it this test would pass
/// against a build that recycled on EVERY send — which would throw away the
/// warm pool on every request, the exact thing `AccountRuntime::http`'s
/// doc-comment forbids.
#[test]
fn the_upstream_client_is_recycled_once_its_serve_budget_is_spent() {
    let manager = manager_of(vec![account("a", 0)]);
    let original = manager.http_client(0).expect("account 0 exists");

    for _ in 0..(MAX_SERVES_PER_CONNECTION - 1) {
        drop(manager.enter_in_flight(0));
    }
    assert!(
        Arc::ptr_eq(
            &original,
            &manager.http_client(0).expect("account 0 exists")
        ),
        "the pool must survive right up to the budget — rebuilding earlier pays a \
         TCP+TLS handshake the keep-alive settings exist to avoid"
    );

    drop(manager.enter_in_flight(0));
    assert!(
        !Arc::ptr_eq(
            &original,
            &manager.http_client(0).expect("account 0 exists")
        ),
        "crossing the serve budget must hand out a NEW client, so the next send \
         opens a fresh h2 connection instead of one about to be drained"
    );
    assert_eq!(
        manager.accounts.read().expect("accounts lock poisoned")[0].serves_since_client_build,
        0,
        "the recycle resets the age counter, so the next budget starts clean"
    );
}

/// After the machine slept, the first send retires EVERY account's client,
/// not only the sending one: every pooled connection outlived the sleep
/// with idle timers that stood still (see `crate::clients::wake`).
///
/// The control half: sends with no sleep between them keep the SAME `Arc`,
/// so this cannot pass against a build that rebuilt on every send; and a
/// second send after the same sleep keeps the client the first one built.
#[test]
fn every_upstream_client_is_retired_on_the_first_send_after_a_sleep() {
    let manager = manager_of(vec![account("a", 0), account("b", 0), account("c", 0)]);
    let clients = |m: &Manager| -> Vec<Arc<reqwest::Client>> {
        (0..3)
            .map(|i| m.http_client(i).expect("account exists"))
            .collect()
    };
    let original = clients(&manager);

    drop(manager.enter_in_flight(0));
    let awake = clients(&manager);
    for (i, (before, after)) in original.iter().zip(&awake).enumerate() {
        assert!(
            Arc::ptr_eq(before, after),
            "account {i}: a send with no sleep before it must keep the warm pool"
        );
    }

    manager
        .wake
        .inject_sleep(std::time::Duration::from_mins(15));
    drop(manager.enter_in_flight(0));
    let woken = clients(&manager);
    for (i, (before, after)) in original.iter().zip(&woken).enumerate() {
        assert!(
            !Arc::ptr_eq(before, after),
            "account {i}: the first send after a sleep must retire this account's \
             client too, not only the sending account's"
        );
    }

    drop(manager.enter_in_flight(1));
    for (i, (before, after)) in woken.iter().zip(&clients(&manager)).enumerate() {
        assert!(
            Arc::ptr_eq(before, after),
            "account {i}: one sleep retires the clients once, not on every later send"
        );
    }
}

/// #6 The SSE fix: once the owned guard is MOVED into a streamed body, the
/// account's `in_flight` stays incremented for the life of the STREAM — not the
/// life of the handler scope — and only decrements when the stream is dropped.
/// A handler-local guard (the bug) would have decremented at handler return,
/// before axum ever polls the body; this asserts the lifetime moved.
#[test]
fn in_flight_guard_lifetime_follows_stream_not_scope() {
    use futures::StreamExt;

    let manager = manager_of(vec![account("a", 0)]);

    // Build the guarded stream exactly as the SSE passthrough does: move the owned
    // guard into a `move` map closure so the stream body OWNS it. The `&guard`
    // anchor mirrors the proxy's forced edition-2021 capture.
    let stream = {
        let guard = manager.enter_in_flight(0);
        assert_eq!(
            manager.accounts.read().expect("accounts lock poisoned")[0].in_flight,
            1,
            "entering bumps the in-flight count"
        );
        futures::stream::iter(0..3u8).map(move |item| {
            let _anchor = &guard;
            item
        })
        // `guard`'s binding scope ENDS here. Were it handler-local, its Drop would
        // fire now and in_flight would fall to 0. It must NOT — the stream owns it.
    };
    assert_eq!(
        manager.accounts.read().expect("accounts lock poisoned")[0].in_flight,
        1,
        "guard moved into the stream: count stays incremented after its binding scope ends"
    );

    drop(stream);
    assert_eq!(
        manager.accounts.read().expect("accounts lock poisoned")[0].in_flight,
        0,
        "dropping the stream drops the guard → the account's in_flight returns to 0"
    );
}

/// #7 A configured in-flight cap of `0` is treated as DISABLED (identical to
/// unset), never as "hold out every account". A literal `Some(0)` would make
/// `in_flight >= 0` true for the whole fleet — pacing out every request, flooding
/// the "skip in selection" log, and collapsing onto the least-loaded fallback.
#[test]
fn in_flight_cap_zero_is_disabled_not_dark_pool() {
    let pacing = PacingConfig {
        max_in_flight_per_account: Some(0),
        min_spacing_ms: None,
    };
    assert_eq!(
        pacing.effective_max_in_flight(),
        None,
        "cap=0 normalises to None (disabled)"
    );
    assert!(
        !pacing.is_active(),
        "cap=0 as the only knob leaves pacing fully inert"
    );

    let manager = build_manager(
        config_with_pacing(vec![account("a", 0), account("b", 0)], pacing),
        counting_refresher(),
    );
    {
        let mut a = manager.accounts.write().expect("accounts lock poisoned");
        a[0].in_flight = 7; // would exceed any positive cap …
        a[1].in_flight = 9;
    }
    let now = OffsetDateTime::now_utc();
    let now_ms = odt_to_ms(now);
    let a = manager.accounts.read().expect("accounts lock poisoned");
    assert!(
        Manager::eligible(
            &a[0],
            manager.global_threshold,
            manager.fable_weekly_threshold,
            &manager.pacing,
            true,
            now,
            now_ms,
            false,
            crate::base::provider::Provider::Anthropic,
            None,
            None,
            &HashSet::new(),
            &HashSet::new(),
        ),
        "cap=0 → account stays eligible regardless of in_flight (no dark pool)"
    );
    // And selection still serves normally rather than flooding the fallback.
    drop(a);
    assert!(
        manager
            .select(&HashSet::new(), now, None, None, "/v1/messages", None)
            .is_some(),
        "cap=0 selects a servable account without pacing anyone out"
    );
}

/// The serving client must never carry a TOTAL request timeout: these responses
/// are long-lived SSE streams, and a total timeout truncates them mid-stream.
/// (The `connect_timeout` it does set is not observable here — `reqwest::Client`
/// exposes no getters and its `Debug` omits it, since the value lives inside the
/// connector. Only the total timeout is surfaced.)
#[test]
fn serving_client_has_no_total_timeout() {
    let manager = manager_of(vec![account("a", 0)]);

    // reqwest prints the total timeout as a `TotalTimeout` field, and only when
    // one is set. Prove the marker on a control client first, so a future reqwest
    // that renames it fails HERE loudly instead of making the guard below pass
    // vacuously.
    let control = crate::base::http_client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("build control client");
    assert!(
        format!("{control:?}").contains("TotalTimeout"),
        "reqwest no longer reports a total timeout in Debug — this guard needs rewriting"
    );

    let accounts = manager.accounts.read().expect("accounts lock poisoned");
    assert!(
        !format!("{:?}", accounts[0].http).contains("TotalTimeout"),
        "the serving client grew a total timeout; it will truncate SSE streams"
    );
}

/// The connection-pool isolation this whole module exists for: two accounts
/// must never be handed the SAME client. `hyper-util` keys its pool on
/// `(scheme, authority)` alone — nothing about the Bearer token or account
/// identity — so a client shared across accounts collapses every account
/// onto one pooled connection, and that connection's death takes every
/// account down with it regardless of how healthy any individual account is
/// (see [`AccountRuntime::http`]'s doc comment for the measured incident).
///
/// This is an IN-PROCESS identity check, not a live network measurement —
/// `Arc::ptr_eq` proves two accounts never share the same client instance
/// (and thus never share its connection pool), and that repeated lookups of
/// the SAME account keep returning the SAME instance rather than a fresh
/// one that would throw away its warm pool. It does not, and cannot, prove
/// two real TCP connections were opened — that would need an actual
/// upstream to observe.
#[test]
fn different_accounts_never_share_a_serving_client() {
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);

    let client_a = manager.http_client(0).expect("account 0 exists");
    let client_b = manager.http_client(1).expect("account 1 exists");
    assert!(
        !Arc::ptr_eq(&client_a, &client_b),
        "two different accounts were handed the same client — a dead \
         connection on one account's pool would take every other account \
         down with it"
    );

    // The SAME account, looked up again, must return the SAME instance —
    // a fresh client per lookup would defeat the whole point (a cold pool
    // on every single request), even though it would still pass the
    // distinctness assertion above.
    let client_a_again = manager.http_client(0).expect("account 0 exists");
    assert!(
        Arc::ptr_eq(&client_a, &client_a_again),
        "the same account's client must be stable across lookups, not \
         rebuilt per call"
    );
}

/// Multi-thread stress: RACE `select()` / `enter_in_flight()` / guard-drop
/// against the other account-lock writers (`mark_rate_limited` /
/// `record_served`) across 8 threads on one shared `Arc<Manager>` with pacing
/// ON. This is the race surface ThreadSanitizer watches in CI (the `tsan` job
/// filters on this test name); TSan is unrunnable on arm64-macOS, so under
/// normal `cargo test` this proves two things TSan does not: the harness runs
/// without panicking, and the shared `in_flight` counter is *balanced* — every
/// account returns to `in_flight == 0` once all guards drop. A leak, a
/// double-decrement, or a lost increment across the check-then-act-across-locks
/// path shows up here as a nonzero residual even without instrumentation.
#[test]
fn concurrent_pacing_stress() {
    const THREADS: usize = 8;
    const ITERS: usize = 2000;

    let pacing = PacingConfig {
        max_in_flight_per_account: Some(2),
        min_spacing_ms: None,
    };
    let manager = build_manager(
        config_with_pacing(
            vec![
                account("a0", 0),
                account("a1", 0),
                account("a2", 0),
                account("a3", 0),
            ],
            pacing,
        ),
        counting_refresher(),
    );

    let handles: Vec<_> = (0..THREADS)
        .map(|t| {
            let manager = Arc::clone(&manager);
            std::thread::spawn(move || {
                let empty: HashSet<usize> = HashSet::new();
                for i in 0..ITERS {
                    let now = OffsetDateTime::now_utc();
                    // Vary the session key per thread AND per iteration so
                    // affinity pins spread across accounts and the pin map
                    // churns — maximising concurrent affinity-lock traffic.
                    let session_key = Some((t as u64) << 32 | (i as u64 % 16));
                    if let Some(idx) =
                        manager.select(&empty, now, None, session_key, "/v1/messages", None)
                    {
                        // Take the in-flight slot, hold it briefly to widen the
                        // window where a concurrent select/writer observes a
                        // nonzero count, then drop the guard (decrement).
                        let guard = manager.enter_in_flight(idx);
                        for _ in 0..8 {
                            std::hint::spin_loop();
                        }
                        // Occasionally fire the other account-lock writers so
                        // TSan sees select/enter racing real mutation, not just
                        // the in_flight path.
                        if i % 17 == 0 {
                            manager.record_served(idx, now, session_key, SessionKind::Fallback);
                        }
                        if i % 53 == 0 {
                            // Short hold so accounts recover and stay selectable
                            // for the rest of the run.
                            manager.mark_rate_limited(idx, 1);
                        }
                        drop(guard);
                    }
                }
            })
        })
        .collect();

    for h in handles {
        // A panic in any thread (e.g. a poisoned lock from an unwind under the
        // write-lock) propagates here and fails the test.
        h.join().expect("stress thread panicked");
    }

    // Every guard has dropped, so the counter MUST be balanced back to zero on
    // every account. A nonzero residual is a real concurrency bug (leaked or
    // double-counted in_flight) — surface it, never silence it.
    let accounts = manager.accounts.read().expect("accounts lock poisoned");
    for (idx, account) in accounts.iter().enumerate() {
        assert_eq!(
            account.in_flight, 0,
            "account {idx} ({}) leaked in_flight={} after all guards dropped",
            account.name, account.in_flight
        );
    }
}
