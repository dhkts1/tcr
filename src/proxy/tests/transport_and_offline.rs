//! Transport failures and their retries, the offline 503, and misrouted requests refused locally.

use super::*;

/// [`client_request`] carrying a JSON body, as reqwest's `.json(..)` sends it.
fn client_json(path: &str, value: &Value) -> Request {
    let mut req = client_request(Method::POST, path, value.to_string());
    req.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    req
}

/// R2 — ONE transport blip must not disable the whole recovery ladder.
///
/// The counter this asserts on used to be a BOOL (`saw_network_error`) set by any
/// `send()` failure and never cleared, checked BEFORE both the soft-wait and the
/// revalidation-serve. So a single blip on a single account short-circuited every
/// recovery path for the rest of the request and returned a 502 claiming "every
/// account transport-failed" — false, and observed live at 09:16:46 while two
/// accounts had already answered with honest 429s and a third served a 200 1.6s
/// later.
///
/// The scripted sequence is exactly that shape: `a` blips, `b` answers with a
/// durable 429 (an upstream ANSWER — proof the network path works), and `c` is
/// over the SOFT threshold so normal `select` benches it while the
/// revalidation-serve may still use it. Reaching `c` at all requires surviving
/// the check the bool used to fail. Old code: 502. New code: `c` serves a 200.
///
/// The script leads with `1 + MAX_SAME_ACCOUNT_TRANSPORT_RETRIES` blips because
/// a transport failure buys the same account that many retries on fresh
/// connections (`repeated_transport_failures_keep_retrying_the_same_account`),
/// so it takes all of them to bench `a` and reach `b`. The scenario under test
/// is unchanged — one account transport-benched, one real upstream 429, one
/// revalidation serve; only the number of sends it takes to set it up moves
/// with that constant. Too few blips here would leave `a` retried into the 429
/// slot and never exercise the revalidation ladder, which is why the blip count
/// is DERIVED from the constant rather than written out: the last time it was a
/// literal, raising the retry bound silently retargeted this test's assertion
/// from `c` to `b`.
#[tokio::test]
async fn transport_blip_does_not_disable_recovery() {
    // `a` blips through its whole same-account ladder, then the two real answers.
    let mut script: Vec<Option<String>> =
        vec![None; (MAX_SAME_ACCOUNT_TRANSPORT_RETRIES as usize) + 1];
    script.push(Some(raw_429_rejected(120))); // `b` answers, durably 429
    script.push(Some(raw_200())); // `c` serves via revalidation
    let up_addr = spawn_scripted_upstream(script).await;
    let manager = fleet(up_addr, &["a", "b", "c"]);

    // Drive `c` OVER the soft switch threshold (0.90 in `dummy_config`) on the
    // shared weekly window with a reset 48h out, via the same public path a real
    // upstream response takes. `select` now benches it — so attempts 1 and 2 must
    // land on `a`/`b` — while `select_revalidation` still allows it, since an
    // over-threshold `allowed_warning` account is a SOFT block, not a hard one.
    let mut over = HeaderMap::new();
    over.insert(
        "anthropic-ratelimit-unified-7d-utilization",
        HeaderValue::from_static("0.99"),
    );
    over.insert(
        "anthropic-ratelimit-unified-7d-reset",
        HeaderValue::from_str(&(crate::now_ms() / 1000 + 172_800).to_string()).unwrap(),
    );
    over.insert(
        "anthropic-ratelimit-unified-status",
        HeaderValue::from_static("allowed_warning"),
    );
    manager.update_quota(2, &over, 200);

    let status = post_one(Arc::clone(&manager)).await;
    assert_eq!(
        status, 200,
        "one transport blip alongside a real upstream 429 must not synthesize a \
             502 — the recovery ladder still had a servable account"
    );

    let snap = manager.snapshot(OffsetDateTime::now_utc());
    let served: Vec<&str> = snap
        .accounts
        .iter()
        .filter(|a| a.requests > 0)
        .map(|a| a.name.as_str())
        .collect();
    assert_eq!(
        served,
        ["c"],
        "the revalidation-serve account must be the one that served, exactly once"
    );
}

/// R2 over-correction guard — when transport failure really IS the whole story,
/// the 502 stays. Every account hangs up without replying, so no attempt ever
/// reaches an upstream HTTP status and `every_attempt_transport_failed` holds.
/// This is what stops the fix above from turning a genuinely unreachable upstream
/// into a misleading 429.
///
/// The scripted upstream's `None` accepts the connection and reads the request
/// before hanging up — a NON-connect failure (`is_connect() == false`), so this
/// is exactly the "outcome unknown" case: the request may have already reached
/// the upstream. `x-should-retry: false` is the assertion that pins this to the
/// review's minimal fix (network-recovery, A-05) rather than the pre-existing
/// bare 502.
#[tokio::test]
async fn all_transport_failures_still_502() {
    let up_addr = spawn_scripted_upstream(vec![None]).await; // every connection blips
    let manager = fleet(up_addr, &["a", "b"]);

    let (status, headers, _body) = post_one_full(Arc::clone(&manager)).await;
    assert_eq!(
        status, 502,
        "no attempt reached an upstream at all — 502 is the honest verdict"
    );
    assert_eq!(
        headers.get("x-should-retry").and_then(|v| v.to_str().ok()),
        Some("false"),
        "a post-connect transport failure means the request's fate upstream is \
             unknown — the client must not blindly resend it"
    );

    let snap = manager.snapshot(OffsetDateTime::now_utc());
    let total: u64 = snap.accounts.iter().map(|a| a.requests).sum();
    assert_eq!(
        total, 0,
        "a transport failure serves nothing, so nothing counts"
    );
}

/// A transport failure kills a CONNECTION, not an account — reqwest pools by
/// `(scheme, authority)` and the account is only a Bearer header, so it is not
/// in the pool key. Retrying the same account on a fresh connection is
/// therefore the failover on the RIGHT axis, and it keeps the account's warm
/// prompt cache instead of paying a cold prefix for a network blip.
///
/// Same shape as `overloaded_529_retries_the_same_account`: the fleet is LRU,
/// so a fresh fleet picks `a` and any rotation picks `b`. Scripting blip-then-200
/// makes the two behaviours distinguishable in the stats — retry-in-place
/// credits `a`, the old bench-and-rotate credits `b`.
#[tokio::test]
async fn transport_failure_retries_the_same_account_first() {
    let up_addr = spawn_scripted_upstream(vec![
        None,            // attempt 1 — `a`'s connection dies
        Some(raw_200()), // attempt 2 — the SAME account, fresh connection
    ])
    .await;
    let manager = fleet(up_addr, &["a", "b"]);

    let status = post_one(Arc::clone(&manager)).await;
    assert_eq!(status, 200, "the retry must serve the client");

    let snap = manager.snapshot(OffsetDateTime::now_utc());
    let served: Vec<&str> = snap
        .accounts
        .iter()
        .filter(|a| a.requests > 0)
        .map(|a| a.name.as_str())
        .collect();
    assert_eq!(
        served,
        ["a"],
        "a transport blip must retry the account it blipped on — landing on `b` \
             means one dead connection benched a healthy account and threw away its \
             warm prompt cache"
    );
}

/// Issue a real GET over cleartext HTTP/2 (prior knowledge, no TLS) and
/// hand back the real `reqwest::Error`. Mirrors `transport_error_for` in
/// `manager/mod.rs`'s tests: a hand-built stub would prove nothing about
/// what the LIVE h2 error's `source()` chain actually renders.
async fn h2_transport_error_for(url: &str) -> reqwest::Error {
    crate::base::http_client::builder()
        .http2_prior_knowledge()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("client builds")
        .get(url)
        .send()
        .await
        .expect_err("this request cannot succeed")
}

/// A minimal h2 server that resets every stream it opens instead of
/// answering it, by returning `Err` from the service, the shape of the
/// live incident (the connection survives, the STREAM does not). Real
/// `hyper` machinery (`hyper_util::server::conn::auto`, already used by
/// `mitm.rs`), not a hand-rolled frame, so the reset is the one hyper's
/// own h2 layer actually produces.
async fn spawn_stream_resetting_upstream() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let service = hyper::service::service_fn(
                    |_req: hyper::Request<hyper::body::Incoming>| async {
                        Err::<hyper::Response<String>, _>(std::io::Error::other(
                            "reset every stream on purpose",
                        ))
                    },
                );
                hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
                    .serve_connection(hyper_util::rt::TokioIo::new(sock), service)
                    .await
                    .log_debug("serving a test connection");
            });
        }
    });
    addr
}

/// **Watch it fail**: before `classify_transport_failure` exists, this does
/// not compile. After the fix, a real HTTP/2 stream reset, the exact shape
/// of the live incident ("the connection stays open and pooled"), must
/// classify as `StreamReset`, and a genuine CONNECT-phase failure (nothing
/// ever opened) must not.
#[tokio::test]
async fn stream_reset_classifies_distinctly_from_connect_phase() {
    let up_addr = spawn_stream_resetting_upstream().await;
    let reset = h2_transport_error_for(&format!("http://{up_addr}/v1/messages")).await;
    println!(
        "reset case: reason={:?} is_connect={} err={reset:?}",
        classify_transport_failure(&reset),
        reset.is_connect()
    );
    assert!(
        !reset.is_connect(),
        "precondition: a stream reset happens AFTER the connection exists"
    );
    assert_eq!(
        classify_transport_failure(&reset),
        TransportFailureReason::StreamReset,
        "an h2 stream reset must classify as StreamReset, not lumped in with a \
             connection-level death: {reset:?}"
    );

    let connect = h2_transport_error_for(&format!(
        "http://127.0.0.1:{}/v1/messages",
        closed_loopback_port()
    ))
    .await;
    println!(
        "connect case: reason={:?} is_connect={} err={connect:?}",
        classify_transport_failure(&connect),
        connect.is_connect()
    );
    assert!(
        connect.is_connect(),
        "precondition: nothing is listening, so this never leaves the connect phase"
    );
    assert_eq!(
        classify_transport_failure(&connect),
        TransportFailureReason::ConnectPhase,
        "a CONNECT failure must not be treated as a stream reset: {connect:?}"
    );
}

/// The relay's own client is retired on the first relay after the machine
/// slept, like the per-account ones, so a relayed path does not answer the
/// client a 502 for a connection the sleep killed. The control half: with
/// no sleep, the relay keeps the SAME client and its warm pool.
///
/// This is the only test that injects into `RELAY_CLIENT`'s detector, and
/// it takes its control pair before injecting; another test relaying in
/// between can only consume the wake itself, which still leaves a new client.
#[test]
fn the_relay_client_is_retired_on_the_first_relay_after_a_sleep() {
    let first = relay_client().expect("the relay client builds");
    let second = relay_client().expect("the relay client builds");
    assert!(
        Arc::ptr_eq(&first, &second),
        "with no sleep between two relays they must share one client"
    );

    RELAY_CLIENT
        .detector()
        .inject_sleep(Duration::from_mins(15));
    let woken = relay_client().expect("the relay client builds");
    assert!(
        !Arc::ptr_eq(&first, &woken),
        "the first relay after a sleep must get a NEW client, not the pool the \
             sleep killed"
    );
}

/// Each recycle logs its own reason. The stream-reset recycle used to log
/// its own line AND the serve-budget one, so the log claimed a budget that
/// was never spent. The serve-budget half is the positive control: the
/// capture can see that wording when it is due.
#[test]
fn each_client_recycle_logs_only_its_own_reason() {
    let manager = Manager::with_live_refresher(dummy_config(None, "http://127.0.0.1:1"), None);

    let reset_log = SharedBuf::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(reset_log.clone())
        .with_ansi(false)
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        manager.recycle_client_after_reset(0);
    });
    let reset_log = reset_log.contents();
    assert!(
        reset_log.contains("after an HTTP/2 stream reset"),
        "the reset recycle names the reset: {reset_log}"
    );
    assert!(
        !reset_log.contains("serve budget"),
        "the reset recycle must not claim a spent serve budget: {reset_log}"
    );

    let budget_log = SharedBuf::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(budget_log.clone())
        .with_ansi(false)
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        for _ in 0..10_000 {
            drop(manager.enter_in_flight(0));
            if budget_log.contents().contains("serve budget") {
                break;
            }
        }
    });
    let budget_log = budget_log.contents();
    assert_eq!(
        budget_log.matches("after its serve budget").count(),
        1,
        "spending the serve budget logs the budget line once: {budget_log}"
    );
    assert!(
        !budget_log.contains("stream reset"),
        "the budget recycle must not claim a stream reset: {budget_log}"
    );
}

/// **The seam the arm actually calls**: `recycle_client_after_reset` swaps
/// the account's client `Arc` (so the retry that follows genuinely lands on
/// a fresh connection), and only the `StreamReset` arm calls it, a
/// `ConnectPhase` failure must leave the client untouched, matching the live
/// arm's own gating (`err.is_connect()` never reaches the recycle call).
#[tokio::test]
async fn recycle_client_after_reset_swaps_the_arc_only_when_called() {
    let up_addr = spawn_stream_resetting_upstream().await;
    let manager = fleet(up_addr, &["a"]);

    let before = manager.http_client(0).expect("account 0 exists");
    let reset = h2_transport_error_for(&format!("http://{up_addr}/v1/messages")).await;
    assert_eq!(
        classify_transport_failure(&reset),
        TransportFailureReason::StreamReset
    );
    manager.recycle_client_after_reset(0);
    let after_reset = manager.http_client(0).expect("account 0 exists");
    assert!(
        !Arc::ptr_eq(&before, &after_reset),
        "a stream reset's recycle must hand the next attempt a DIFFERENT client, \
             or it checks out the same broken connection again"
    );

    // A ConnectPhase failure never reaches the recycle call in the live arm
    // (mirrored here rather than re-driving the whole proxy loop): the
    // client must be untouched.
    let connect = h2_transport_error_for(&format!(
        "http://127.0.0.1:{}/v1/messages",
        closed_loopback_port()
    ))
    .await;
    assert_eq!(
        classify_transport_failure(&connect),
        TransportFailureReason::ConnectPhase
    );
    let after_connect = manager.http_client(0).expect("account 0 exists");
    assert!(
        Arc::ptr_eq(&after_reset, &after_connect),
        "a CONNECT failure must never recycle a client that was never pooled"
    );
}

/// A REPEATED blip still keeps the account, up to
/// [`MAX_SAME_ACCOUNT_TRANSPORT_RETRIES`]. This is the case the old
/// one-retry bound got wrong: it rotated on the second failure, on the
/// theory that a second failure is evidence about the account. Live
/// measurement (2026-08-27) says it is evidence about the CONNECTION — the
/// first same-account retry alone lands a 200 in 94.7% of cases — and
/// rotating instead throws away a warm prompt cache for a 5-10x cold one.
///
/// Three blips then a 200, and the whole request must still be served by
/// `a`: the fleet is LRU, so any rotation would show up as `b`.
#[tokio::test]
async fn repeated_transport_failures_keep_retrying_the_same_account() {
    let (up_addr, attempts) = spawn_counted_upstream(vec![
        None,            // attempt 1 — `a`'s connection dies
        None,            // attempt 2 — the fresh connection dies too
        None,            // attempt 3 — and again
        Some(raw_200()), // attempt 4 — still `a`, and it serves
    ])
    .await;
    let manager = fleet(up_addr, &["a", "b"]);

    let status = post_one(Arc::clone(&manager)).await;
    assert_eq!(status, 200, "the third retry must still serve the client");

    let snap = manager.snapshot(OffsetDateTime::now_utc());
    let served: Vec<&str> = snap
        .accounts
        .iter()
        .filter(|a| a.requests > 0)
        .map(|a| a.name.as_str())
        .collect();
    assert_eq!(
        served,
        ["a"],
        "a run of connection blips must not bench a healthy account — landing \
             on `b` means the aging-connection fault cost us a cold prefix"
    );
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        4,
        "one send plus three same-account retries, all on `a`"
    );
}

/// The other half of the bound: the ladder is finite. Once an account has
/// spent [`MAX_SAME_ACCOUNT_TRANSPORT_RETRIES`] it IS benched and the request
/// rotates, so a genuinely broken account cannot hold a request forever.
///
/// The attempt count is read from the upstream rather than inferred: exactly
/// five sends (`a` four times, then `b`). A sixth would mean the same-account
/// ladder is unbounded, which is how a permanently blipping upstream would
/// burn the whole `max_attempts` budget on one account.
#[tokio::test]
async fn transport_failures_past_the_retry_budget_bench_the_account_and_rotate() {
    let (up_addr, attempts) = spawn_counted_upstream(vec![
        None,            // attempt 1 — `a` blips
        None,            // attempt 2 — retry 1
        None,            // attempt 3 — retry 2
        None,            // attempt 4 — retry 3, the budget is now spent
        Some(raw_200()), // attempt 5 — rotated to `b`
    ])
    .await;
    let manager = fleet(up_addr, &["a", "b"]);

    let status = post_one(Arc::clone(&manager)).await;
    assert_eq!(status, 200, "`b` still serves the request");

    let snap = manager.snapshot(OffsetDateTime::now_utc());
    let served: Vec<&str> = snap
        .accounts
        .iter()
        .filter(|a| a.requests > 0)
        .map(|a| a.name.as_str())
        .collect();
    assert_eq!(
        served,
        ["b"],
        "`a` is benched once its transport-retry budget is spent"
    );
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        (MAX_SAME_ACCOUNT_TRANSPORT_RETRIES as usize) + 2,
        "one send plus the full retry budget on `a`, then exactly one rotation"
    );
}

/// A CONNECT failure is not retried on the same account.
///
/// The retry exists because a pooled connection can die between requests: the
/// error itself evicts it, so the retry gets a fresh one and the account —
/// which was never the failing resource — keeps its warm prompt cache. None
/// of that holds when the connect itself failed: there was no pooled
/// connection, nothing was evicted, and the second send is the identical
/// operation. What it does buy is a second full `connect_timeout` (10s), and
/// on a blackholed route (no RST, no reply) that doubles every request's
/// worst-case time-to-502 — ~80s to ~160s on an eight-account fleet — with a
/// per-account in-flight slot held for the whole of it.
///
/// Read off the 503 the client actually receives, whose body carries the
/// attempt count: two accounts that each fail to connect ONCE is `2`, and the
/// retry firing here would make it `4`. An unreachable upstream is a dead
/// port — the one shape that produces `is_connect()` for real rather than by
/// simulation. 503, not 502: every attempt died at connect, so nothing ever
/// left this box and retrying is free — see [`bad_gateway`].
#[tokio::test]
async fn a_connect_failure_is_not_retried_on_the_same_account() {
    // Bind then drop: the address is guaranteed to have been free, and now
    // refuses. `spawn_counted_upstream`'s no-reply script cannot produce this
    // — dropping an ACCEPTED socket is a mid-request error, not a connect one.
    let dead = format!("127.0.0.1:{}", closed_loopback_port());
    let manager = fleet(dead, &["a", "b"]);

    let (status, headers, body) = post_one_full(manager).await;
    assert_eq!(
        status, 503,
        "nothing reached an upstream, and every attempt died at connect — the honest \
             answer is a recoverable gateway error: {body}"
    );
    assert!(
        body.contains("all 2 attempt(s)"),
        "one connect attempt per account. `all 4 attempt(s)` means each was \
             retried into a second connect timeout for no new information: {body}"
    );
    assert_eq!(
        headers.get("x-should-retry").and_then(|v| v.to_str().ok()),
        Some("true"),
        "nothing ever left this box, so telling the client to retry costs nothing extra"
    );
    assert_eq!(
        headers.get("retry-after").and_then(|v| v.to_str().ok()),
        Some(OFFLINE_RETRY_AFTER_SECS.to_string().as_str()),
        "the client needs guidance on when to come back"
    );
}

/// The MIXED ladder — a transport blip AND the 529 walk on one request — must
/// fit the rotation loop's budget.
///
/// This replaces a guard that could not fail. The previous version asserted
/// `accounts * 2 <= max_attempts_for(accounts)` against a formula of
/// `2n + 4`, i.e. `2n <= 2n + 4`: true by construction for every input and
/// for every fleet size, so it passed unchanged through the very change it
/// was written to guard. A gate that cannot go red is not a gate.
///
/// The ladder below is derived from the LADDER constants instead, so it is
/// an independent claim about the loop and not a restatement of the budget:
///
/// * Each of the (up to `1 + MAX_529_FAILOVERS_PER_REQUEST`) accounts on the
///   529 walk can spend `1` send, `1` transport retry, and
///   `MAX_SAME_ACCOUNT_529_RETRIES` in-place retries before it fails over.
/// * Every remaining account in the fleet is still reachable by the plain
///   transport ladder, at `1` send plus `1` retry each.
///
/// Under the old `2n + 4` this is false from three accounts up (12 sends
/// against a budget of 10) — which is the truncation
/// [`the_mixed_ladder_forwards_the_real_529_instead_of_synthesizing_a_429`]
/// shows a client actually receiving.
#[test]
fn the_mixed_transport_and_529_ladder_fits_the_attempt_budget() {
    let walked = 1 + MAX_529_FAILOVERS_PER_REQUEST as usize;
    for accounts in 1..=16usize {
        let on_the_529_walk = accounts.min(walked);
        let sends = on_the_529_walk * (2 + MAX_SAME_ACCOUNT_529_RETRIES as usize)
            + (accounts - on_the_529_walk) * 2;
        assert!(
            sends <= max_attempts_for(accounts),
            "a {accounts}-account fleet can spend {sends} sends on a blip-plus-529 \
                 ladder but the loop allows only {} attempts — the walk is truncated \
                 mid-request and the client gets a synthesized 429",
            max_attempts_for(accounts)
        );
    }
}

/// THE CLIENT-VISIBLE CONSEQUENCE of the budget above, end to end through
/// `handle`: a fleet where every account blips once and is then overloaded.
///
/// Each account spends 4 sends (blip, retry, then the two in-place 529
/// backoffs) before failing over, so the full three-account walk is 12 —
/// against the old budget of `2*3 + 4 = 10`. Truncated, the loop falls out
/// mid-walk, `every_attempt_transport_failed` is false because upstreams did
/// respond, and the client is handed `exhausted_response`: HTTP 429 "All 3
/// accounts exhausted" with a FABRICATED retry-after, for a request whose
/// honest answer is the upstream's own 529. The status is the assertion; the
/// attempt count is what proves the whole mixed ladder actually ran rather
/// than the test having accidentally taken a shorter path to the same code.
#[tokio::test]
async fn the_mixed_ladder_forwards_the_real_529_instead_of_synthesizing_a_429() {
    let blip_then_overloaded = || vec![None, Some(raw_529()), Some(raw_529()), Some(raw_529())];
    let script: Vec<Option<String>> = (0..3).flat_map(|_| blip_then_overloaded()).collect();
    assert_eq!(
        script.len(),
        12,
        "three accounts of blip + a full 529 ladder"
    );

    let (up_addr, attempts) = spawn_counted_upstream(script).await;
    let manager = fleet(up_addr, &["a", "b", "c"]);

    let (status, body) = post_one_with_body(manager).await;
    assert_eq!(
        status, 529,
        "the upstream's own 529 must reach the client, not a 429 we invented \
             because the loop ran out of attempts: {body}"
    );
    assert!(
        !body.contains("exhausted"),
        "a synthesized exhaustion body means the walk was truncated: {body}"
    );
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        12,
        "every rung of the mixed ladder has to have been walked — a smaller \
             number means the request stopped early and the 529 above was luck"
    );
}

/// Issue a real request that cannot succeed and hand back the real
/// `reqwest::Error`. A hand-built stub would prove nothing here: the whole
/// question is what the LIVE error's `source()` chain looks like.
async fn transport_error_for(url: &str) -> reqwest::Error {
    crate::base::http_client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("client builds")
        .get(url)
        .send()
        .await
        .expect_err("this request cannot succeed")
}

/// A port nothing is listening on: bind it, read it back, release it.
fn closed_loopback_port() -> u16 {
    crate::test_support::loopback::unused_port()
}

/// The two-sided gate on [`is_offline_error`]. One side alone is worthless —
/// a classifier that answers `true` to everything passes the offline half and
/// would then hold every dead route on its pinned account forever.
///
/// Both errors below are `is_connect() == true`, which is precisely why the
/// arm cannot be gated on `is_connect()`: that is the bug being fixed.
#[tokio::test]
async fn is_offline_error_separates_a_dead_resolver_from_a_dead_route() {
    // TRUE case: `.invalid` is reserved by RFC 2606 to never resolve.
    let dns = transport_error_for("http://offline-probe.invalid/v1/messages").await;
    println!(
        "offline case: is_offline={} is_connect={} err={dns:?}",
        is_offline_error(&dns),
        dns.is_connect()
    );
    assert!(
        dns.is_connect(),
        "precondition: a resolver failure is is_connect, so is_connect cannot discriminate"
    );
    assert!(
        is_offline_error(&dns),
        "a name-resolution failure must classify as offline: {dns:?}"
    );

    // FALSE case: the host resolves and answers with an RST. That IS evidence
    // about the route, so it must keep taking the rotate arm.
    let url = format!("http://127.0.0.1:{}/v1/messages", closed_loopback_port());
    let refused = transport_error_for(&url).await;
    println!(
        "refused case: is_offline={} is_connect={} err={refused:?}",
        is_offline_error(&refused),
        refused.is_connect()
    );
    assert!(
        refused.is_connect(),
        "precondition: a refused connection is is_connect too"
    );
    assert!(
        !is_offline_error(&refused),
        "connection refused to a resolvable host is a ROUTE failure, not an offline \
             machine — classifying it offline would pin a request to a dead account: {refused:?}"
    );
}

/// End to end: an upstream whose hostname cannot resolve must answer `503`
/// with a `retry-after`, never the `502` this branch used to produce.
#[tokio::test]
async fn an_unresolvable_upstream_answers_503_with_retry_after_not_502() {
    let manager =
        Manager::with_live_refresher(dummy_config(None, "http://offline-probe.invalid"), None);
    let resp = send_to_proxy(
        app(manager),
        client_json(
            "/v1/messages",
            &serde_json::json!({"model": "claude-3-5-sonnet", "messages": []}),
        ),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "an offline machine is a recoverable local condition, not a bad gateway"
    );
    assert_eq!(
        resp.headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok()),
        Some(OFFLINE_RETRY_AFTER_SECS.to_string().as_str()),
        "the 503 must tell the client when to come back"
    );
}

/// Relay one raw `POST /v1/oauth/token` to `upstream` and return the answer,
/// calling [`relay_upstream`] directly so nothing else in [`handle`] decides
/// the status.
async fn relay_to(upstream: &str) -> Response {
    let manager = Manager::with_live_refresher(dummy_config(None, upstream), None);
    relay_upstream(
        &manager,
        RelayMode::Raw,
        Provider::Anthropic,
        Method::POST,
        "/v1/oauth/token",
        &HeaderMap::new(),
        Body::from(r#"{"grant_type":"refresh_token"}"#),
        Instant::now(),
    )
    .await
}

/// A relay whose connection is refused never left this box, so it answers
/// the main path's safe-to-retry 503, not a 502 that points at the upstream.
#[tokio::test]
async fn a_relay_refused_at_connect_answers_503_with_retry_after() {
    let resp = relay_to(&format!("http://127.0.0.1:{}", closed_loopback_port())).await;
    assert_eq!(
        resp.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "a connect-phase relay failure sent nothing upstream, so it is a 503"
    );
    assert_eq!(
        retry_after(&resp),
        Some(OFFLINE_RETRY_AFTER_SECS.to_string().as_str()),
        "the connect-phase 503 tells the client when to come back"
    );
}

/// A relay whose upstream host cannot resolve is the machine being offline,
/// and answers the main path's offline 503.
#[tokio::test]
async fn a_relay_to_an_unresolvable_host_answers_the_offline_503() {
    let resp = relay_to("http://offline-probe.invalid").await;
    assert_eq!(
        resp.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "a dead resolver is a local, recoverable condition, not a bad gateway"
    );
    assert_eq!(
        retry_after(&resp),
        Some(OFFLINE_RETRY_AFTER_SECS.to_string().as_str()),
        "the offline 503 tells the client when to come back"
    );
    let body = to_bytes(resp.into_body(), MAX_BODY_BYTES)
        .await
        .expect("read body");
    assert!(
        String::from_utf8_lossy(&body).contains("Name resolution failed"),
        "the offline answer, not the connect-phase one: {}",
        String::from_utf8_lossy(&body)
    );
}

/// A relay that got PAST connect may have reached the upstream, so it keeps
/// today's plain 502 with no `x-should-retry` either way: its outcome is
/// unknown, and whether relayed paths should tell clients to retry is not
/// decided here.
#[tokio::test]
async fn a_relay_dropped_after_connect_still_answers_502() {
    // Read the request so it has provably left the client, then hang up
    // without answering.
    let addr = spawn_scripted_upstream(vec![None]).await;

    let resp = relay_to(&format!("http://{addr}")).await;
    assert_eq!(
        resp.status(),
        StatusCode::BAD_GATEWAY,
        "a failure past connect keeps the 502: the request may have arrived"
    );
    assert_eq!(
        resp.headers().get("x-should-retry"),
        None,
        "the relay's 502 says nothing about retrying, as before"
    );
    assert_eq!(retry_after(&resp), None, "and carries no retry-after");
}

/// The per-request wait bound must stay inside the loop's total attempt
/// budget, for the smallest fleet there can be — otherwise a request that
/// rides out a resolver blip is truncated mid-walk.
#[test]
fn the_offline_wait_bound_fits_the_attempt_budget() {
    assert!(
        (MAX_OFFLINE_WAITS_PER_REQUEST as usize) < max_attempts_for(1),
        "offline waits ({MAX_OFFLINE_WAITS_PER_REQUEST}) must not exhaust the attempt budget"
    );
}

/// Test 1 — a forward-proxy IMDS probe in ABSOLUTE-form (the shape an AWS SDK
/// emits when `HTTP_PROXY` points at tcr) is rejected locally with 421, read
/// from the request-target's authority. 421 (not the 503 a real forward to the
/// dead upstream would give) proves the guard fired BEFORE any egress.
#[tokio::test]
async fn misroute_absolute_form_imds_rejected_locally_421() {
    let addr = spawn_dead_upstream_proxy().await;
    let status = raw_request_status(
        addr,
        "PUT http://169.254.169.254/latest/api/token HTTP/1.1\r\n\
             Host: 169.254.169.254\r\n\
             Content-Length: 0\r\n\
             Connection: close\r\n\r\n",
    )
    .await;
    assert_eq!(
        status, 421,
        "an absolute-form misroute must be rejected locally (421), never forwarded (would be 502)"
    );
}

/// Test 2 — the same misroute in ORIGIN-form, the target host carried only in
/// the `Host` header, is likewise rejected with 421. Exercises the guard's
/// Host-header fallback branch.
#[tokio::test]
async fn misroute_host_header_imds_rejected_421() {
    let addr = spawn_dead_upstream_proxy().await;
    let status = raw_request_status(
        addr,
        "PUT /latest/api/token HTTP/1.1\r\n\
             Host: 169.254.169.254\r\n\
             Content-Length: 0\r\n\
             Connection: close\r\n\r\n",
    )
    .await;
    assert_eq!(
        status, 421,
        "a Host-header misroute must also be rejected locally (421)"
    );
}

/// Test 3 — FALSE-REJECT REGRESSION GATE (non-negotiable). A legitimate
/// base-URL request (origin-form `/v1/messages`, loopback `Host`) MUST pass
/// the guard and reach the rotation loop → dead upstream → 503 (connection
/// refused is a pure connect-phase failure, so it is the safe-to-retry shape —
/// see [`bad_gateway`]). A 421 here would mean the guard wrongly rejected real
/// Anthropic traffic.
#[tokio::test]
async fn base_url_loopback_proceeds_not_rejected_503() {
    let resp = send_to_proxy(
        app(dead_upstream_manager()),
        client_request(Method::POST, "/v1/messages", "{}"),
    )
    .await;
    assert_eq!(
        resp.status().as_u16(),
        503,
        "the guard must let a loopback base-URL request through (dead upstream → 503), never 421"
    );
    assert_eq!(
        resp.headers()
            .get("x-should-retry")
            .and_then(|v| v.to_str().ok()),
        Some("true"),
        "a pure connect-phase failure never reached an upstream, so retrying is free"
    );
}

/// Test 4 — the MITM-terminated shape (origin-form `/v1/messages`, `Host:
/// api.anthropic.com`) passes the guard via the allowlist branch
/// (`crate::base::tls::host_allowed`) → rotation loop → dead upstream → 503.
#[tokio::test]
async fn anthropic_host_proceeds_not_rejected_503() {
    let addr = spawn_dead_upstream_proxy().await;
    let status = raw_request_status(
        addr,
        "POST /v1/messages HTTP/1.1\r\n\
             Host: api.anthropic.com\r\n\
             Content-Length: 2\r\n\
             Connection: close\r\n\r\n{}",
    )
    .await;
    assert_eq!(
        status, 503,
        "an api.anthropic.com request must pass the guard (dead upstream → 503), never 421"
    );
}

/// Test 5 — the path-shape guard over a REAL socket, with the request target
/// written byte for byte. The `drive` harness builds its URI through `http::Uri`
/// in-process; this proves the same shapes survive hyper's own request-line
/// parser and are refused there too, so the guard is not an artifact of the test
/// harness. 400 — not the 503 a forwarded request to the dead upstream gives —
/// is what proves it fired BEFORE any egress.
#[tokio::test]
async fn traversal_targets_refused_over_a_raw_socket_400() {
    let addr = spawn_dead_upstream_proxy().await;
    for target in [
        "/v1/code/../../v1/messages",
        "/v1/code/%2e%2e/%2e%2e/v1/messages",
        "/x/../_tcr/status",
        "/v1/code/..\\../v1/messages",
        "/v1/code\\foo",
    ] {
        let status = raw_request_status(
            addr,
            &format!(
                "POST {target} HTTP/1.1\r\n\
                     Host: api.anthropic.com\r\n\
                     Content-Length: 2\r\n\
                     Connection: close\r\n\r\n{{}}"
            ),
        )
        .await;
        assert_eq!(
            status, 400,
            "{target} must be refused locally, never forwarded (503)"
        );
    }
    // The control: the same socket, the same dead upstream, an ordinary path.
    // 503 proves the guard rejects the ambiguous shape and nothing else.
    let status = raw_request_status(
        addr,
        "POST /v1/messages HTTP/1.1\r\n\
             Host: api.anthropic.com\r\n\
             Content-Length: 2\r\n\
             Connection: close\r\n\r\n{}",
    )
    .await;
    assert_eq!(
        status, 503,
        "an ordinary path still reaches the rotation loop (dead upstream → 503)"
    );
}
