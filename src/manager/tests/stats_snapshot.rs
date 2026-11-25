//! The stats snapshot: row status, sessions, request counters, and usage totals.

use super::*;

#[test]
fn account_uuid_accessor_returns_configured_value() {
    let mut with_uuid = account("has-uuid", 0);
    with_uuid.account_uuid = Some("abcdef01-2345-6789-abcd-ef0123456789".to_string());
    let without_uuid = account("no-uuid", 0);
    let manager = manager_of(vec![with_uuid, without_uuid]);

    assert_eq!(
        manager.account_uuid(0).as_deref(),
        Some("abcdef01-2345-6789-abcd-ef0123456789"),
        "configured account_uuid is returned"
    );
    assert_eq!(manager.account_uuid(1), None, "absent config → None");
    assert_eq!(manager.account_uuid(99), None, "out-of-range idx → None");
}

/// Change 3: an account near/over its weekly quota is held out of rotation but
/// stays operationally ACTIVE — never a red "error". The snapshot exposes this
/// as a `quota_state` (`NearLimit` / Exhausted) while `status` remains "active".
#[test]
fn near_and_over_quota_render_active_with_honest_quota_state() {
    use crate::records::stats::QuotaState;

    let manager = manager_of(vec![account("near", 0), account("full", 0)]);
    let now = OffsetDateTime::now_utc();
    set_7d(&manager, 0, "0.95"); // over the 0.90 threshold, under 100% → NearLimit
    set_7d(&manager, 1, "1.20"); // over 100% → Exhausted

    let snap = manager.snapshot(now);
    // Both are held out of rotation …
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
        None,
        "over-threshold accounts are held out of rotation"
    );
    // … yet neither is an "error": status stays active, quota_state is honest.
    assert_eq!(snap.accounts[0].status, "active");
    assert_eq!(snap.accounts[1].status, "active");
    assert_eq!(snap.accounts[0].quota_state, QuotaState::NearLimit);
    assert_eq!(snap.accounts[1].quota_state, QuotaState::Exhausted);
}

/// Biting test for the 0.90 → 0.95 default-threshold raise. Keyed off the
/// SHIPPED config default (parsed from a minimal config), not a hardcoded
/// literal, so it tracks the constant: an account at 0.92 weekly utilization
/// has real headroom under the 0.95 default → stays Normal and eligible. On
/// the pre-change tree (default 0.90) that same 0.92 tripped the line → held
/// `NearLimit` and `select()` would refuse it, so this fails before the raise.
/// 0.96 → `NearLimit` (held, credential fine); 1.00 → Exhausted (fully spent).
#[test]
fn default_threshold_raise_gives_92pct_headroom_normal_near_full() {
    use crate::records::stats::QuotaState;

    // The default the shipped config applies when `switchThreshold` is absent.
    let default_threshold = serde_json::from_str::<Config>(r#"{ "accounts": [] }"#)
        .expect("minimal config parses")
        .switch_threshold;
    let mut config = config_with(vec![
        account("headroom", 0),
        account("near", 0),
        account("full", 0),
    ]);
    config.switch_threshold = default_threshold;
    let manager = build_manager(config, counting_refresher());
    let now = OffsetDateTime::now_utc();
    set_7d(&manager, 0, "0.92"); // under the 0.95 default → real headroom, Normal
    set_7d(&manager, 1, "0.96"); // over 0.95, under 100% → NearLimit
    set_7d(&manager, 2, "1.00"); // at 100% → Exhausted

    let snap = manager.snapshot(now);
    assert_eq!(
        snap.accounts[0].quota_state,
        QuotaState::Normal,
        "0.92 has headroom under the 0.95 default (pre-change 0.90 held it NearLimit)"
    );
    assert_eq!(snap.accounts[1].quota_state, QuotaState::NearLimit);
    assert_eq!(snap.accounts[2].quota_state, QuotaState::Exhausted);
    // The headroom account is not merely labelled Normal — it is actually
    // servable (pre-change it was held out and select() skipped it).
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
        Some(0),
        "the 0.92 account is eligible under the raised default"
    );
}

/// Best-practices review (Apollo ch7 — no stale shadow of a live value): the
/// `Throttled` enum is cleared only when the account next serves a non-429, so a
/// naturally-expired hold would show a STALE "throttled" in the snapshot while
/// routing already treats the account as eligible (routing reads the live
/// `rate_limited_until_ms`, not the enum). The snapshot must derive the displayed
/// status from the live deadline, like the quota bars and `rate_limited_until`.
#[test]
fn snapshot_status_is_active_once_the_throttle_hold_expires() {
    let manager = manager_of(vec![account("live-hold", 0), account("expired-hold", 0)]);
    let now = OffsetDateTime::now_utc();

    manager.mark_rate_limited(0, 300); // a LIVE hold → still "throttled"
    manager.mark_rate_limited(1, 300); // mark, then rewind the deadline into the past:
    {
        let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
        accounts[1].rate_limited_until_ms = Some(crate::now_ms() - 1_000);
    }

    let snap = manager.snapshot(now);
    assert_eq!(
        snap.accounts[0].status, "throttled",
        "a live hold still displays throttled"
    );
    assert_eq!(
        snap.accounts[1].status, "active",
        "an expired hold must display active, not a stale throttled — routing already \
         treats it as eligible"
    );
    assert!(
        snap.accounts[1].rate_limited_until.is_none(),
        "the live rate_limited_until is already filtered to future-only"
    );
}

/// Bug #4: the request counter increments ONCE per client request, not once
/// per upstream response. A request that folds quota headers from several
/// upstream responses (a retry that rotated accounts) still counts one.
#[test]
fn request_counter_counts_once_across_retries() {
    let manager = manager_of(vec![account("a", 0)]);
    let now = OffsetDateTime::now_utc();

    // Simulate two upstream responses for a single client request (e.g. a
    // 429 that recorded a spent bucket, then the retry's 200).
    let headers = reqwest::header::HeaderMap::new();
    manager.update_quota(0, &headers, 429);
    manager.update_quota(0, &headers, 200);
    // Exactly one terminal serve.
    manager.record_served(0, now, None, SessionKind::Fallback);

    let snap = manager.snapshot(now);
    assert_eq!(snap.accounts[0].requests, 1);
    assert_eq!(snap.current, Some(0));
}

/// `record_served`'s `kind` threads through the per-session stat into the
/// snapshot, so the TUI can fold fallback sessions without touching routing.
/// A stable serve persists `SessionKind::Stable`; a fallback serve `Fallback`.
#[test]
fn record_served_threads_stable_flag_into_snapshot() {
    let manager = manager_of(vec![account("a", 0)]);
    let now = OffsetDateTime::now_utc();

    // Two distinct session keys: one stable-identity serve, one fallback.
    manager.record_served(0, now, Some(0xABCD), SessionKind::Stable);
    manager.record_served(0, now, Some(0x1234), SessionKind::Fallback);

    let snap = manager.snapshot(now);
    let stable = snap
        .sessions
        .iter()
        .find(|s| s.id == short_session_id(0xABCD))
        .expect("stable session present in snapshot");
    let fallback = snap
        .sessions
        .iter()
        .find(|s| s.id == short_session_id(0x1234))
        .expect("fallback session present in snapshot");
    assert_eq!(
        stable.kind,
        SessionKind::Stable,
        "stable serve must persist SessionKind::Stable"
    );
    assert_eq!(
        fallback.kind,
        SessionKind::Fallback,
        "fallback serve must persist SessionKind::Fallback"
    );
}

/// Session `key`'s row in the manager's snapshot at `now`.
fn session_in_the_snapshot(manager: &Manager, now: OffsetDateTime, key: u64) -> SessionSnapshot {
    manager
        .snapshot(now)
        .sessions
        .into_iter()
        .find(|s| s.id == short_session_id(key))
        .expect("the session is present in the snapshot")
}

/// The sessions pane must report the PIN, never whoever happened to serve the
/// last request. A hold that clears while the pinned account's prompt cache is
/// still warm DIVERTS one request and deliberately keeps the pin (see
/// [`CACHE_WARM_HOLD_SECS`]) — but the snapshot used to take its account from the
/// SERVING index, so every divert made the session visibly jump accounts in the
/// TUI although the pin never moved. A display that misreports state is worse
/// than no display: it made a fleet measured at a 1.70% switch rate read as
/// "sessions keep jumping".
#[test]
fn snapshot_reports_the_pin_not_the_last_serve() {
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    let now = OffsetDateTime::now_utc();
    let key = 0xFEEDu64;

    // Pin the session to `a`, and serve one request there.
    assert_eq!(
        manager.select(&HashSet::new(), now, None, Some(key), "/v1/messages", None),
        Some(0),
        "precondition: the first select pins the session to `a`"
    );
    manager.record_served(0, now, Some(key), SessionKind::Stable);

    // `a` picks up a SHORT hold — one that clears while its cache is still warm.
    {
        let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
        accounts[0].rate_limited_until_ms = Some(odt_to_ms(now) + SHORT_HOLD_SECS * 1000);
    }
    assert_eq!(
        manager.select(&HashSet::new(), now, None, Some(key), "/v1/messages", None),
        Some(1),
        "precondition: the short hold DIVERTS this one request to `b`"
    );
    manager.record_served(1, now, Some(key), SessionKind::Stable);

    let session = session_in_the_snapshot(&manager, now, key);
    assert_eq!(
        session.account, "a",
        "the snapshot must report the PIN, which a divert never moves"
    );
    assert_eq!(
        session.last_served_account, "b",
        "the divert must stay OBSERVABLE in its own field — the goal is honesty, \
         not concealment"
    );
}

/// The other half of the same contract, so the fix cannot swing too far and start
/// hiding genuine re-keys behind a stale pin: when the pin DURABLY moves, the
/// snapshot follows it. Only an ACCOUNT-level hard gate re-keys, so sideline `a`
/// and let the next select re-pin the session to `b`.
#[test]
fn snapshot_account_follows_a_real_rekey() {
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    let now = OffsetDateTime::now_utc();
    let key = 0xBEEFu64;

    assert_eq!(
        manager.select(&HashSet::new(), now, None, Some(key), "/v1/messages", None),
        Some(0),
        "precondition: the session starts pinned to `a`"
    );
    manager.record_served(0, now, Some(key), SessionKind::Stable);

    // A dead credential is ACCOUNT-level death → the pin is durably re-keyed.
    manager.mark_error(0);
    assert_eq!(
        manager.select(&HashSet::new(), now, None, Some(key), "/v1/messages", None),
        Some(1),
        "precondition: an errored pin re-keys the session to `b`"
    );
    manager.record_served(1, now, Some(key), SessionKind::Stable);

    let session = session_in_the_snapshot(&manager, now, key);
    assert_eq!(
        session.account, "b",
        "a REAL re-key must move the reported account — the pin is the authority \
         in both directions"
    );
    assert_eq!(session.last_served_account, "b");
}

/// Serving a request must not re-order the sessions pane. Rows used to sort
/// most-recent-first, so every single request threw its session to the top and
/// the pane churned under the operator's eyes — the other half of why a stable
/// fleet looked like it was thrashing.
#[test]
fn session_rows_do_not_reorder_on_a_serve() {
    let manager = manager_of(vec![account("a", 0)]);
    let now = OffsetDateTime::now_utc();
    let keys = [0x11u64, 0x22, 0x33];

    // Three sessions pinned to the one account, last seen 3s/2s/1s ago — so a
    // recency order and a stable order are genuinely different orders here.
    for key in keys {
        manager.select(&HashSet::new(), now, None, Some(key), "/v1/messages", None);
    }
    for (offset, key) in [3i64, 2, 1].into_iter().zip(keys) {
        manager.record_served(
            0,
            now - Duration::seconds(offset),
            Some(key),
            SessionKind::Stable,
        );
    }

    let ids = |snap: StatsSnapshot| -> Vec<String> {
        snap.sessions.iter().map(|s| s.id.clone()).collect()
    };
    let before = ids(manager.snapshot(now));
    assert_eq!(
        before.len(),
        3,
        "precondition: all three sessions are shown"
    );

    // The OLDEST session — not the first row — serves one request.
    manager.record_served(0, now, Some(keys[0]), SessionKind::Stable);
    let after = ids(manager.snapshot(now));

    assert_eq!(
        before, after,
        "a serve must never move a session's row; the age belongs in the `Last` \
         column, not in the row order"
    );
}

/// Cache tokens count: `update_usage` accumulates whatever the caller sums,
/// which for the proxy includes cache-creation + cache-read input tokens.
#[test]
fn update_usage_accumulates_input_and_output() {
    let manager = manager_of(vec![account("a", 0)]);
    // e.g. input_tokens=10 + cache_creation=100 + cache_read=1000 = 1110.
    // The last two args are the cache-read / cache-creation SUBSETS of that sum.
    manager.update_usage(0, 1110, 42, 1000, 100);
    manager.update_usage(0, 0, 8, 0, 0);
    let snap = manager.snapshot(OffsetDateTime::now_utc());
    // R1: input_tokens is the SUM, byte-identical to the pre-cache behaviour.
    assert_eq!(snap.accounts[0].input_tokens, 1110);
    assert_eq!(snap.accounts[0].output_tokens, 50);
    // NEW: the cache components accumulate separately (a subset of the sum).
    assert_eq!(snap.accounts[0].cache_read_tokens, 1000);
    assert_eq!(snap.accounts[0].cache_creation_tokens, 100);
}

/// FINDING 7. `input_tokens` is the QUOTA counter, and `update_usage` is
/// `pub` on a library crate — so it must add the caller's number, verbatim,
/// whatever relationship that number has to the components passed beside
/// it. Decomposing with a saturating subtraction and re-summing the pieces
/// grew the counter by 800 for an `input_tokens` of 100, which is both the
/// quota consumption and the denominator of `cacheHitRatio`.
#[test]
fn update_usage_adds_the_callers_input_verbatim() {
    let manager = manager_of(vec![account("a", 0)]);
    // An inconsistent caller: 100 of input against 800 of components.
    manager.update_usage(0, 100, 5, 750, 50);
    let snap = manager.snapshot(OffsetDateTime::now_utc());
    assert_eq!(
        snap.accounts[0].input_tokens, 100,
        "the quota counter grows by exactly what the caller passed"
    );
    assert_eq!(snap.accounts[0].output_tokens, 5);
    assert_eq!(snap.accounts[0].cache_read_tokens, 750);
    assert_eq!(snap.accounts[0].cache_creation_tokens, 50);
}

/// `next_session_key` hands out strictly-increasing, unique u64s starting at 1.
#[test]
fn session_keys_are_monotonic_and_unique() {
    let manager = manager_of(vec![account("a", 0)]);
    let mut prev = 0u64;
    let mut seen = HashSet::new();
    for i in 0..5 {
        let key = manager.next_session_key();
        if i == 0 {
            assert_eq!(key, 1, "the first session key is 1");
        }
        assert!(key > prev, "session keys must strictly increase");
        assert!(seen.insert(key), "session keys must be unique");
        prev = key;
    }
}
