// ---------------------------------------------------------------------------
// A borrowed request served end to end: the dry-fleet seam, the lender's own
// account, the headers it forwards and the debit it charges
// ---------------------------------------------------------------------------

use super::fixtures::*;
use super::*;

/// **The dry-fleet arm reaches the configured provider, and returns what it
/// answered.**
///
/// This is the wiring gate: a real `mitm::serve` in front of a `Manager` with
/// no account that can serve, one POST, and a provider that counts. Before
/// `configured_provider` read an installed provider it returned `None`
/// unconditionally, and this test could not distinguish "the seam is wired" from
/// "the seam exists".
///
/// It is the only test in this binary that installs a provider, because the
/// install is process-wide by design (see `fallback::PROVIDER`): a second
/// installer in the same binary would race this one.
///
/// Watch it fail by making `configured_provider` return `None`.
#[tokio::test(flavor = "multi_thread")]
async fn a_dry_fleet_reaches_the_configured_provider() {
    /// What the provider answers with, so the body the client reads proves
    /// WHERE the answer came from rather than merely that one arrived.
    const SENTINEL: &[u8] = b"{\"served_by\":\"the-test-provider\"}";

    struct Counting {
        asked: Arc<std::sync::atomic::AtomicUsize>,
        paths: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl FallbackProvider for Counting {
        fn name(&self) -> &'static str {
            "test-counting"
        }
        fn try_serve<'a>(
            &'a self,
            ask: &'a Ask<'a>,
        ) -> futures::future::BoxFuture<'a, Option<axum::response::Response>> {
            Box::pin(async move {
                self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                self.paths
                    .lock()
                    .expect("paths lock")
                    .push(ask.path.to_string());
                Some(
                    axum::response::Response::builder()
                        .status(200)
                        .header("content-type", "application/json")
                        .body(axum::body::Body::from(SENTINEL.to_vec()))
                        .expect("build the provider's answer"),
                )
            })
        }
    }

    let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let paths = Arc::new(std::sync::Mutex::new(Vec::new()));
    assert!(
        fallback::install_provider(Box::new(Counting {
            asked: Arc::clone(&asked),
            paths: Arc::clone(&paths),
        })),
        "nothing else in this binary may install a provider"
    );

    let proxy = fleet::spawn_proxy(fleet::dry_manager()).await;
    let client = tcr::base::http_client::builder()
        .no_proxy()
        .build()
        .expect("a loopback client");
    let response = client
        .post(format!("{proxy}/v1/messages"))
        .header("content-type", "application/json")
        .body(r#"{"model":"claude-sonnet-4-5","messages":[]}"#)
        .send()
        .await
        .expect("the proxy answered");

    assert_eq!(response.status().as_u16(), 200);
    let body = response.bytes().await.expect("read the body");
    assert_eq!(
        body.as_ref(),
        SENTINEL,
        "the client reads the PROVIDER's answer, not the exhausted 429"
    );
    assert_eq!(
        asked.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the dry-fleet arm consulted the provider exactly once"
    );
    assert_eq!(
        paths.lock().expect("paths lock").as_slice(),
        ["/v1/messages".to_string()],
        "the ask carries the request's own path"
    );

    // ---------------------------------------------------------------------
    // **Hole B, executable**: the dry-fleet arm REFUSES TO BUILD an `Ask` for a
    // client-credential path, so no provider is ever offered one: not even a
    // provider that would have said yes to anything.
    //
    // This is the half of the defence that lives at the seam rather than in the
    // provider (`src/proxy.rs:2352`). It was prose and a `.then()` with nothing
    // exercising it: the provider's OWN refusal
    // (`the_provider_declines_a_credential_path_without_consulting_a_lease`)
    // passes whether or not the seam refuses, because the provider refuses
    // again. The instrument that can tell them apart is a provider that refuses
    // NOTHING (`Counting` above), which is why this assertion has to live in
    // the one test allowed to install one.
    //
    // # Which of these paths the seam is actually the ENFORCEMENT point for
    //
    // Measured, because a loop over seven paths reads as seven gates and is
    // not: `relay_mode` (`src/proxy.rs:1052`) intercepts a request before
    // rotation ever runs, so the six `CLIENT_CREDENTIAL_PREFIXES` never reach
    // the picker at all, whatever their method, and neither does a `POST
    // /v1/oauth/token`. For those, the seam's refusal is a backstop against
    // `relay_mode` and this list drifting apart: real, and not the thing this
    // assertion can see.
    //
    // **`GET /v1/oauth/token` is the one shape that reaches the seam.**
    // `relay_mode`'s first arm is `POST`-only, so a non-POST spelling of the
    // seventh path falls through to the picker, comes up dry, and arrives here
    //: where `serve_is_allowed_for_path` is the only thing between a client's
    // own credential exchange and another Mac's process. Mutating the `.then()`
    // to build the `Ask` unconditionally leaves every POST in this loop green
    // and turns this one red, which is how the distinction above was measured
    // rather than reasoned.
    // ---------------------------------------------------------------------
    for path in ["/v1/oauth/token", "/v1/code", "/_tcr/accounts"] {
        let refused = client
            .post(format!("{proxy}{path}"))
            .header("content-type", "application/json")
            .body(r#"{"model":"claude-sonnet-4-5","messages":[]}"#)
            .send()
            .await
            .expect("the proxy answered");
        assert_ne!(
            refused.bytes().await.expect("read the body").as_ref(),
            SENTINEL,
            "{path} was served by a fallback provider; the seam must not build an Ask for it"
        );
    }
    let slipped = client
        .get(format!("{proxy}/v1/oauth/token"))
        .send()
        .await
        .expect("the proxy answered");
    assert_ne!(
        slipped.bytes().await.expect("read the body").as_ref(),
        SENTINEL,
        "a GET to the token-exchange path slipped past `relay_mode` and was relayed to \
         another Mac: this is the shape the seam's own refusal is the enforcement point for"
    );
    assert_eq!(
        asked.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the provider was consulted exactly once, for /v1/messages: a credential path \
         never reaches it, because the seam builds no Ask at all"
    );

    // ---------------------------------------------------------------------
    // **Item 3, the borrower side: a NON-POST never reaches a provider.**
    //
    // `ServeRequest::method` used to be written as the literal `"POST"` by the
    // borrower and read by nobody, so a client that sent `GET /v1/messages`
    // had it POSTed on somebody else's account: an answer to a question it
    // never asked, paid for out of a lease. `handle_serve_on` refuses that
    // frame now, and that refusal is the LENDER's: it happens after the bytes
    // have crossed a host boundary, on a machine whose operator did not ask
    // for them.
    //
    // So the refusal is at the seam, and this is the instrument that can see
    // it: a provider that refuses nothing. `GET /v1/messages` is a path the
    // seam is otherwise happy to relay: the loop above proves the path
    // refusal is about the PATH: so the only thing that can stop it here is
    // the method.
    //
    // The answer the client gets is whatever this handler answered before any
    // of this existed: `exhausted_or_fallback`'s terminal, with no provider
    // consulted. Not an error page invented for the occasion.
    //
    // Watch it fail by dropping `method == axum::http::Method::POST` from the
    // `.then()` in `src/proxy.rs`: `asked` goes to 2 and the body is the
    // provider's sentinel.
    // ---------------------------------------------------------------------
    let non_post = client
        .get(format!("{proxy}/v1/messages"))
        .send()
        .await
        .expect("the proxy answered");
    let status = non_post.status().as_u16();
    assert_ne!(
        non_post.bytes().await.expect("read the body").as_ref(),
        SENTINEL,
        "a GET was relayed to another Mac and would have been POSTed on its account"
    );
    assert_eq!(
        asked.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the provider is still at one ask: a non-POST builds no `Ask` at all"
    );
    assert_eq!(
        status, 429,
        "a non-POST on a dry fleet gets the honest exhausted answer this handler has \
         always given, not something invented for the peer path"
    );

    // ---------------------------------------------------------------------
    // **A HARD ACCOUNT LOCK NEVER REACHES A PROVIDER.**
    //
    // `lock_account` is the operator saying this fleet is one account, and the
    // arm that answers a locked request already refuses the revalidation serve
    // for exactly that reason: it must not send the request through another
    // POOLED credential. It was then changed to call `exhausted_or_fallback`,
    // which consults a provider that never reads the lock, so a request pinned
    // to one account was served on a PEER's credential the moment its own
    // account could not serve: a wider escape than the one the line above it
    // refuses, because the credential is not even this Mac's.
    //
    // A SECOND proxy in this test rather than a second test, because the
    // provider install is process-wide and this is the one test allowed to do
    // it. The instrument is the same one: a provider that says yes to
    // anything, so the only thing that can produce a 429 here is the lock.
    //
    // Watch it fail by removing the `locked` conjunct from
    // `exhausted_or_fallback` (`src/proxy.rs`): the body is the sentinel, the
    // status is 200 and `asked` goes to 2.
    // ---------------------------------------------------------------------
    let (rejecting, rejections) = fleet::spawn_rejecting_upstream().await;
    let locked_proxy = fleet::spawn_proxy(fleet::locked_manager(&rejecting)).await;
    let refused = client
        .post(format!("{locked_proxy}/v1/messages"))
        .header("content-type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .body(r#"{"model":"claude-sonnet-4-5","messages":[]}"#)
        .send()
        .await
        .expect("the locked proxy answered");
    let locked_status = refused.status().as_u16();
    assert_ne!(
        refused.bytes().await.expect("read the body").as_ref(),
        SENTINEL,
        "a request pinned to ONE account was served on a peer's credential"
    );
    assert_eq!(
        locked_status, 429,
        "a locked account that cannot serve gets the honest exhausted answer, not a \
         borrow: the lock has no failover, local or remote"
    );
    assert_eq!(
        asked.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the provider is still at one ask: a locked fleet never consults one"
    );
    assert!(
        rejections.load(std::sync::atomic::Ordering::SeqCst) >= 1,
        "the locked account really did try and really was refused, which is the \
         condition this arm is about"
    );

    // ---------------------------------------------------------------------
    // **A REQUEST THAT HAS ALREADY CROSSED ONE MAC IS NEVER BORROWED ONWARD.**
    //
    // A relayed request is served by re-posting it onto the lender's own
    // loopback proxy, and that proxy is this one: a dry fleet there walks to
    // the same terminal and consults the same provider. So on a mesh where
    // everyone is dry, a borrow could be borrowed onward, to a third Mac or
    // back to the Mac that sent it, each hop locally reasonable and the cycle
    // visible only from outside. The lender marks the request it re-posts
    // (`peer::serve::serve_on_own_account`), and the seam refuses to build an
    // `Ask` for a marked one.
    //
    // The instrument is the same provider that says yes to anything, which is
    // the only thing that can tell "the seam refused" from "the provider
    // refused", and it is why this leg lives in the one test allowed to
    // install one.
    //
    // Watch it fail by dropping the `RELAYED_HEADER_NAME` conjunct from the
    // `.then()` in `src/proxy.rs`: the body is the sentinel and `asked` goes
    // to 2.
    // ---------------------------------------------------------------------
    let relayed = client
        .post(format!("{proxy}/v1/messages"))
        .header("content-type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .header(tcr::proxy::RELAYED_HEADER_NAME, "1")
        .body(r#"{"model":"claude-sonnet-4-5","messages":[]}"#)
        .send()
        .await
        .expect("the proxy answered");
    let relayed_status = relayed.status().as_u16();
    assert_ne!(
        relayed.bytes().await.expect("read the body").as_ref(),
        SENTINEL,
        "a request that had already crossed one Mac was borrowed onward: one hop is the \
         rule, and a mesh of dry Macs would otherwise pass it around"
    );
    assert_eq!(
        asked.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the provider is still at one ask: a relayed request builds no `Ask` at all"
    );
    assert_eq!(
        relayed_status, 429,
        "and it gets the honest exhausted answer this handler has always given, which is \
         what the borrower's own lender then reports"
    );
}

/// **The two-process gate, on the REAL listener: a borrowed request is served
/// on the LENDER's account.**
///
/// One lender and one borrower in this binary, on kernel ports, with a fake
/// upstream behind the lender's own proxy. What answers the borrower is
/// `listener::serve_on_with`: the shipped accept loop, handshake, stream gate
/// and dispatch arm. That used not to be true: `StreamKind::Serve` was a
/// `bail!` and this test drove a harness shaped like the arm that was missing,
/// so it proved the two halves worked and not that anything joined them.
///
/// Three assertions, each a separate claim this design makes:
///
/// 1. the borrower's client reads the canned body: the relay works end to end,
///    through the production dispatch;
/// 2. the fake upstream was called with the LENDER's Bearer: the lender's own
///    picker and its own credential served it, which is also what pays the
///    lender's per-organization GCRA bucket (see the pacing gate below);
/// 3. the lease was debited exactly once and the in-flight slot was released.
///
/// # And a fourth: **this test goes red when the scrub is deleted**
///
/// It did not, and the brief's item 2 is that it must. The credential claim was
/// moved out to `a_relayed_frame_the_lender_receives_holds_no_credential`
/// because it used to live here as an absence in a CIPHERTEXT stream over a
/// request built from an EMPTY header map: an assertion that could not go red
/// however the scrub was broken. Moving it left this test named for the happy
/// path and silent about the thing that makes the happy path safe.
///
/// Two changes bring it back as a measurement. The borrower's own credentials
/// are in scope for the real leg above, so assertion 2 is about a request that
/// HAD a token to leak; and the same ask is then relayed a second time to a
/// CAPTURING lender, which keeps the frame it decrypted: the one place a
/// borrower's plaintext request exists on the lender's side. The real listener
/// deliberately exposes it nowhere, and exposing it would mean a production
/// seam that hands a borrower's plaintext to a caller.
///
/// Watch it fail by returning the dispatch arm to its `bail!`: assertion 1 goes
/// red with the lender closing the stream. Watch assertion 2 fail by pointing
/// `handle_serve_on` at anything other than the lender's own proxy: the Bearer
/// is the thing the lender's own path substitutes. Watch assertion 4 fail by
/// deleting the `scrub_client_credentials(&mut scrubbed)` line from
/// `serve_request_from`: all four secrets are then in the captured frame.
/// Measured red that way before this leg was kept.
#[tokio::test(flavor = "multi_thread")]
async fn a_borrowed_request_is_served_on_the_lenders_account() {
    let (upstream, hits, served) = fleet::spawn_upstream().await;
    let lender_proxy =
        fleet::spawn_proxy(fleet::lending_manager(&upstream, Default::default())).await;

    // A lender whose own fleet has never been probed. The rise is then
    // unmeasurable and `Ledger::debit` charges MIN_DEBIT, which is what
    // assertion 3 reads: see `a_served_request_debits_the_measured_rise` for
    // the other half.
    let ServeRig {
        pair,
        lease,
        ledger,
        peer_addr,
        store: borrower_store,
    } = serve_rig(
        LendGrant::new(Window::SevenDay, 0.20, 300, 2),
        lender_proxy.clone(),
    )
    .await;
    let (lender_id, borrower_id, borrower_peers) =
        (pair.lender_id, pair.borrower_id, &pair.borrower_peers);

    // The borrower's own client's headers, credentials included. An EMPTY map
    // here is what made this test blind to the scrub: nothing could leak, so
    // nothing being leaked proved nothing.
    let headers = borrower_credentials();
    let ask = ask_for("/v1/messages");
    let response = serve::open_serve(&lender_id, &lease, &ask, &headers, &borrower_store)
        .await
        .expect("the SERVE stream ran")
        .served()
        .expect("the lender served it");

    assert_eq!(response.status().as_u16(), 200);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("read the relayed body");
    assert_eq!(
        body.as_ref(),
        fleet::CANNED,
        "the borrower's client reads exactly what the lender's account answered"
    );

    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    let seen = served.lock().expect("served lock").clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0].token,
        fleet::LENDER_TOKEN,
        "the request was served on the LENDER's own credential"
    );
    // The borrower's secrets were in scope for that request, so their absence
    // from what the lender sent upstream is a measurement. Over VALUES and not
    // over header names: a secret arriving under a name nobody thought to check
    // is invisible to a per-name assertion.
    for secret in BORROWER_SECRETS {
        assert!(
            !seen[0].values().iter().any(|value| value.contains(secret)),
            "{secret} reached the fake upstream on the lender's own TLS session: {:?}",
            seen[0].headers
        );
    }

    // Assertion 3 in a block of its own: the ledger guard must not be alive
    // across assertion 4's `.await`s below, and a `drop` at the end of a long
    // body is a line the next edit moves without noticing.
    {
        let held = ledger.lock().expect("ledger lock");
        let after = held
            .live(tcr::now_ms())
            .first()
            .copied()
            .expect("the lease is still live");
        assert!(
            (after.spent - lease::MIN_DEBIT).abs() < f64::EPSILON,
            "one relayed request, one debit, got {}",
            after.spent
        );
        assert_eq!(
            held.inflight(LEASE),
            0,
            "the in-flight slot is released, or the lease is stranded for its whole TTL"
        );
    }

    // ASSERTION 4, and the one that makes this test go red when the scrub is
    // deleted: the SAME ask, the SAME headers, relayed to a lender that keeps
    // the frame it decrypted.
    //
    // Why a second lender rather than an assertion on the first: the lender
    // above copies only `LENDER_FORWARDED_HEADERS` onto its own outbound
    // request, so a borrower credential that survived the scrub is dropped one
    // hop later and the upstream assertion stays green. That allowlist is a
    // backstop and a good one: and it is exactly what MASKS a broken scrub
    // from every assertion that reads the far end. The frame is the only place
    // the scrub's effect is visible.
    let capture_home = tempfile::tempdir().expect("the capturing lender's temp home");
    let capture_peers = capture_home.path().join("tcr-peers.json");
    let capture_id = tcr::peer::config::id::NodeKey::load_or_mint(capture_home.path())
        .expect("the capturing lender's key")
        .id();
    write_peers(
        &capture_peers,
        vec![lender_row_for(
            borrower_id,
            vec![LendGrant::new(Window::SevenDay, 0.20, 300, 2)],
        )],
    );
    let captured: mesh::CapturedFrames = Arc::new(std::sync::Mutex::new(Vec::new()));
    let capture_addr = mesh::spawn_capturing_lender(
        capture_home.path().to_path_buf(),
        capture_peers,
        Arc::clone(&captured),
    )
    .await;
    write_peers(
        borrower_peers,
        vec![
            borrower_row_for(lender_id, true, vec![peer_addr.to_string()]),
            borrower_row_for(capture_id, true, vec![capture_addr.to_string()]),
        ],
    );
    let borrower_store = PeerStore::open(borrower_peers).expect("the borrower's peers file");
    serve::open_serve(&capture_id, &lease, &ask, &headers, &borrower_store)
        .await
        .expect("the second SERVE stream ran")
        .served()
        .expect("the capturing lender answered");

    let frames = captured.lock().expect("captured-frames lock").clone();
    assert_eq!(frames.len(), 1, "one request, one captured frame");
    assert_frame_holds_no_credential(&String::from_utf8_lossy(&frames[0]));
}

/// **The lender forwards an ALLOWLIST of headers and drops everything else** :
/// the hole testing proved, driven by a borrower that is malicious
/// on purpose.
///
/// The lender replayed every borrower-supplied header onto the request it makes
/// through its own proxy. `build_upstream_headers` (`src/proxy.rs:3502`) then
/// removes `authorization`, `x-api-key`, `accept-encoding` and the hop-by-hop
/// names on the way upstream: and `cookie` is none of those, so a
/// borrower-chosen session cookie went to Anthropic on the lender's own TLS
/// session, as did any header a borrower invented.
///
/// The instrument is the LENDER's outbound request, recorded by the fake
/// upstream: not the frame the borrower built (the borrower here is hostile and
/// builds it by hand), and not the lender's own logs.
///
/// Watch it fail by restoring the replay loop in `serve_on_own_account`
/// (`for (name, value) in &request.headers`): `cookie`, `x-borrower-invented`
/// and the borrower's `user-agent`-shaped secrets all appear upstream. Measured
/// red that way before this test was kept.
#[tokio::test(flavor = "multi_thread")]
async fn a_lender_forwards_only_the_allowlisted_headers() {
    let (upstream, hits, served) = fleet::spawn_upstream().await;
    let lender_proxy =
        fleet::spawn_proxy(fleet::lending_manager(&upstream, Default::default())).await;

    let pair = hostile_pair();
    let peer_addr = pair.spawn_lender(lender_proxy).await;

    // Every name the brief names, plus one nobody thought of: which is the
    // whole argument for an allowlist over a denylist.
    let hostile = vec![
        (
            "authorization".to_string(),
            "Bearer not-a-real-token".to_string(),
        ),
        (
            "cookie".to_string(),
            "sessionKey=not-a-real-cookie".to_string(),
        ),
        ("x-api-key".to_string(), "not-a-real-key".to_string()),
        (
            "proxy-authorization".to_string(),
            "Bearer not-a-real-proxy-key".to_string(),
        ),
        (
            "host".to_string(),
            "not-a-real-host.example.com".to_string(),
        ),
        (
            "x-borrower-invented".to_string(),
            "not-a-real-smuggle".to_string(),
        ),
        // The positive control: one allowlisted header, so the absences below
        // are about the filter and not about a request that carried nothing.
        ("content-type".to_string(), "application/json".to_string()),
    ];
    let answered = hostile_serve(
        peer_addr,
        &pair.lender_key,
        &pair.borrower_key,
        serve::ServeRequest {
            headers: hostile,
            ..hostile_request("POST", "/v1/messages")
        },
    )
    .await;
    assert!(
        answered.is_ok(),
        "the lender serves this request; it is the HEADERS it must drop, not the request"
    );

    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the request really was served, so the assertions below are about the filter"
    );
    let seen = served.lock().expect("served lock").clone();
    let sent = &seen[0];

    // The positive control first: if this is absent the request went out bare
    // and every absence below is free.
    assert_eq!(
        sent.header("content-type"),
        Some("application/json"),
        "an allowlisted header IS forwarded: {:?}",
        sent.headers
    );
    assert_eq!(
        sent.token,
        fleet::LENDER_TOKEN,
        "the Bearer upstream is the LENDER's, never the borrower's"
    );

    for dropped in [
        "cookie",
        "x-api-key",
        "proxy-authorization",
        "x-borrower-invented",
    ] {
        assert_eq!(
            sent.header(dropped),
            None,
            "{dropped} reached Anthropic on the lender's session: {:?}",
            sent.headers
        );
    }
    // `host` is set by the lender's own HTTP client for the host it dials, so
    // the claim is not that the name is absent: it is that the borrower's
    // VALUE is not what it says.
    assert_ne!(
        sent.header("host"),
        Some("not-a-real-host.example.com"),
        "the borrower chose the lender's Host header"
    );
    // And the strong form, on values rather than names: a secret that arrived
    // under a name nobody thought to check is invisible to every assertion
    // above.
    for secret in [
        "not-a-real-token",
        "not-a-real-cookie",
        "not-a-real-key",
        "not-a-real-proxy-key",
        "not-a-real-smuggle",
        "not-a-real-host.example.com",
    ] {
        assert!(
            !sent.values().iter().any(|value| value.contains(secret)),
            "{secret} reached Anthropic inside a header value: {:?}",
            sent.headers
        );
    }
}

/// **A non-POST relayed request is refused, never silently `POSTed`.**
///
/// `ServeRequest::method` was written as the literal `"POST"` by the borrower
/// and read by nobody, so a borrower whose client sent `GET /v1/messages` had
/// it `POSTed` on the lender's account: an answer to a question it never asked,
/// paid for out of a lease. The field is honoured, this build serves exactly
/// one method and refuses the rest rather than rewriting them.
///
/// Watch it fail by deleting the method check in `handle_serve_on`: the lender
/// then answers 200 and the fake upstream's hit count goes to 1.
#[tokio::test(flavor = "multi_thread")]
async fn a_non_post_relayed_request_is_refused() {
    let (upstream, hits, _served) = fleet::spawn_upstream().await;
    let lender_proxy =
        fleet::spawn_proxy(fleet::lending_manager(&upstream, Default::default())).await;

    let pair = hostile_pair();
    let peer_addr = pair.spawn_lender(lender_proxy).await;

    let answered = hostile_serve(
        peer_addr,
        &pair.lender_key,
        &pair.borrower_key,
        hostile_request("GET", "/v1/messages"),
    )
    .await;

    assert!(
        answered.is_err(),
        "the lender closes on a method it does not serve rather than answering it"
    );
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "nothing was sent upstream: a GET is refused, not turned into a POST"
    );
}

/// **The relayed frame the LENDER decrypted carries no credential of the
/// borrower's**: with the borrower's token in scope, which is what makes the
/// absence a measurement.
///
/// This is the assertion `a_borrowed_request_is_served_on_the_lenders_account`
/// used to make and could not: `open_serve` built its frame from
/// `&HeaderMap::new()` inline, so the scrub ran over an empty map, and the
/// bytes it asserted on were Noise ciphertext. Deleting
/// `scrub_client_credentials` from `serve_request_from` left it green.
///
/// Both halves are fixed here. `open_serve` takes the borrower's headers, this
/// test puts four credential-shaped secrets in them, and the lender captures
/// the frame it DECRYPTED: the one place a borrower's plaintext request
/// exists on the lender's side.
///
/// Watch it fail by deleting the `scrub_client_credentials(&mut scrubbed)` line
/// in `serve_request_from` (`src/peer/serve.rs`): all four secrets are then in
/// the captured frame. Measured red that way before this test was kept.
#[tokio::test(flavor = "multi_thread")]
async fn a_relayed_frame_the_lender_receives_holds_no_credential() {
    let pair = lease_pair(LendGrant::new(Window::SevenDay, 0.50, 300, 5));

    let captured: mesh::CapturedFrames = Arc::new(std::sync::Mutex::new(Vec::new()));
    let peer_addr = mesh::spawn_capturing_lender(
        pair.lender_home.path().to_path_buf(),
        pair.lender_peers.clone(),
        Arc::clone(&captured),
    )
    .await;
    let store = open_borrower_store(&pair.borrower_peers, pair.lender_id, peer_addr);

    // The borrower's own client's headers, credentials included, which is the
    // case `crate::peer::serve::fallback`'s `Ask` carries none of TODAY and the case the
    // scrub exists for. The same map
    // `a_borrowed_request_is_served_on_the_lenders_account` uses, from one
    // place: two lists of credential shapes is two places for one of them to
    // go missing.
    let headers = borrower_credentials();

    let lease = live_lease(0.50, 5);
    let ask = ask_for("/v1/messages");
    let response = serve::open_serve(&pair.lender_id, &lease, &ask, &headers, &store)
        .await
        .expect("the SERVE stream ran")
        .served()
        .expect("the capturing lender answered");
    assert_eq!(response.status().as_u16(), 200);

    let frames = captured.lock().expect("captured-frames lock").clone();
    assert_eq!(frames.len(), 1, "one request, one captured frame");
    assert_frame_holds_no_credential(&String::from_utf8_lossy(&frames[0]));
}

/// **A served request moves the ledger by the MEASURED utilization rise**, not
/// by `MIN_DEBIT`.
///
/// `handle_serve_on` debited through `Ledger::debit(.., 0.0)`: always
/// `MIN_DEBIT`, a constant whose own doc calls itself a guess, because the
/// delta needs the serving account's utilization before and after and the
/// signature held no way to read one. It reads the window through
/// `WindowUtilization` either side of the serve now, and
/// `serve::utilization_rise` turns the pair into the charge.
///
/// The reader is scripted rather than a real fleet: a real rise needs the fake
/// upstream's headers to move the lender's quota between two probes, which
/// tests `update_from_headers` rather than the debit. What is asserted is that
/// the figure the reader reported is the figure the lease was charged.
///
/// Watch it fail by restoring `ledger.debit(request.lease_id,
/// request.request_id, 0.0)`: `spent` is then `MIN_DEBIT` (0.002) against the
/// 0.07 asserted here. Measured red that way before this test was kept.
#[tokio::test(flavor = "multi_thread")]
async fn a_served_request_debits_the_measured_utilization_rise() {
    let (upstream, hits, _served) = fleet::spawn_upstream().await;
    let lender_proxy =
        fleet::spawn_proxy(fleet::lending_manager(&upstream, Default::default())).await;

    let pair = lease_pair(LendGrant::new(Window::SevenDay, 0.50, 300, 5));
    let lease = live_lease(0.50, 5);
    let ledger = shared_ledger(Some((lease, pair.borrower_id)));

    // Two accounts, and the SECOND is the one that moved: 0.10 -> 0.17. The
    // first is flat, so a reader that returned a fleet maximum instead of an
    // elementwise rise would answer 0.40 - 0.40 = 0.0 and charge MIN_DEBIT.
    let utilization = Arc::new(mesh::ScriptedUtilization::new(vec![
        vec![Some(0.40), Some(0.10)],
        vec![Some(0.40), Some(0.17)],
    ]));
    let peer_addr = mesh::spawn_lender(
        pair.lender_home.path().to_path_buf(),
        pair.lender_peers.clone(),
        Arc::clone(&ledger),
        lender_proxy,
        Arc::<mesh::ScriptedUtilization>::clone(&utilization),
        fleet::dry_manager(),
    )
    .await;
    let store = open_borrower_store(&pair.borrower_peers, pair.lender_id, peer_addr);

    let ask = ask_for("/v1/messages");
    serve::open_serve(&pair.lender_id, &lease, &ask, &HeaderMap::new(), &store)
        .await
        .expect("the SERVE stream ran")
        .served()
        .expect("the lender served it");

    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(
        utilization.reads_taken(),
        2,
        "the window is read once before the serve and once after; a reader nobody \
         called would satisfy the charge assertion by accident"
    );

    let held = ledger.lock().expect("ledger lock");
    let after = held
        .live(tcr::now_ms())
        .first()
        .copied()
        .expect("the lease is still live");
    assert!(
        (after.spent - 0.07).abs() < 1e-9,
        "the lease is charged the 0.07 the lender measured, not MIN_DEBIT ({}); got {}",
        lease::MIN_DEBIT,
        after.spent
    );
}

/// The arithmetic behind the debit above, without a socket.
///
/// Four claims, and each one is a direction a scalar figure would get wrong.
#[test]
fn a_utilization_rise_is_the_largest_single_accounts_and_never_a_credit() {
    // The largest SINGLE account's rise, never the sum: one request is served
    // by one account.
    assert!(
        (serve::utilization_rise(&[Some(0.10), Some(0.20)], &[Some(0.13), Some(0.24)]) - 0.04)
            .abs()
            < 1e-9
    );

    // A window that reset under the request is the OWNER's windfall, never the
    // borrower's credit.
    assert_eq!(
        serve::utilization_rise(&[Some(0.90)], &[Some(0.01)]),
        0.0,
        "a reset is not a credit"
    );

    // An index unmeasured on either side contributes nothing: a rise is a
    // difference between two measurements and there is only one here.
    assert_eq!(serve::utilization_rise(&[None], &[Some(0.40)]), 0.0);
    assert_eq!(serve::utilization_rise(&[Some(0.40)], &[None]), 0.0);

    // A fleet that answered nothing at all: `NoFleetUtilization`, and a
    // lender whose accounts vector is empty.
    assert_eq!(serve::utilization_rise(&[], &[]), 0.0);

    // A fleet that gained an account between the two reads is read over the
    // pairs that exist, rather than panicking on the length mismatch.
    assert!(
        (serve::utilization_rise(&[Some(0.10)], &[Some(0.15), Some(0.99)]) - 0.05).abs() < 1e-9
    );
}

/// **An answer larger than one frame is CARRIED, and it is charged.**
///
/// This test used to assert the opposite, and the re-review named that as the
/// defect: the fix before this one made an over-cap answer a readable 502
/// instead of a dead stream, but it left `MAX_BODY_BYTES` (65 519 bytes) as the
/// ceiling on a whole answer. Claude Code always streams, so every borrowed
/// answer of any length was bought on the lender's account, debited, and handed
/// back to the borrower as an error with no content.
///
/// A body travels as a run of frames terminated by an empty one now, so the
/// size of an answer is no longer a thing the borrower can be refused over.
/// Both halves are measured: the client gets the WHOLE body, byte for byte, and
/// the lease is charged the rise the lender measured (0.07, from the scripted
/// reader).
///
/// Watch it fail by restoring the one-frame refusal in `serve_on_own_account`
/// (`if body.len() > MAX_BODY_BYTES { return Ok(undeliverable_answer(..)) }`):
/// the status is 502 and the body is the refusal's own JSON rather than the
/// answer's bytes.
#[tokio::test(flavor = "multi_thread")]
async fn an_answer_larger_than_one_frame_is_carried_and_charged() {
    // One byte over what a frame carries: the smallest reply that reaches the
    // branch, so the test cannot pass because the body was enormous.
    let (upstream, hits) = fleet::spawn_huge_upstream(serve::MAX_BODY_BYTES + 1).await;
    let lender_proxy =
        fleet::spawn_proxy(fleet::lending_manager(&upstream, Default::default())).await;

    let pair = lease_pair(LendGrant::new(Window::SevenDay, 0.50, 300, 5));
    let lease = live_lease(0.50, 5);
    let ledger = shared_ledger(Some((lease, pair.borrower_id)));
    let utilization = Arc::new(mesh::ScriptedUtilization::new(vec![
        vec![Some(0.10)],
        vec![Some(0.17)],
    ]));
    let peer_addr = mesh::spawn_lender(
        pair.lender_home.path().to_path_buf(),
        pair.lender_peers.clone(),
        Arc::clone(&ledger),
        lender_proxy,
        Arc::<mesh::ScriptedUtilization>::clone(&utilization),
        fleet::dry_manager(),
    )
    .await;
    let store = open_borrower_store(&pair.borrower_peers, pair.lender_id, peer_addr);

    let ask = ask_for("/v1/messages");
    let answer = serve::open_serve(&pair.lender_id, &lease, &ask, &client_headers(), &store)
        .await
        .expect("a reply over one frame must not kill the SERVE stream")
        .served()
        .expect("the borrower is answered rather than left to re-ask elsewhere");
    assert_eq!(
        answer.status().as_u16(),
        200,
        "an answer over one frame is carried to the client, not refused back to it"
    );
    let carried = axum::body::to_bytes(answer.into_body(), 64 * 1024 * 1024)
        .await
        .expect("the carried answer's body reads");
    assert_eq!(
        carried.len(),
        serve::MAX_BODY_BYTES + 1,
        "every byte of the answer arrives, across as many frames as it took"
    );
    assert!(
        carried.iter().all(|byte| *byte == b'x'),
        "the body is the origin's own bytes and not a refusal's JSON"
    );
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the lender's own account really did pay for this request"
    );

    let held = ledger.lock().expect("ledger lock");
    let spent = held
        .live(tcr::now_ms())
        .first()
        .map(|lease| lease.spent)
        .expect("the lease is still live");
    assert!(
        (spent - 0.07).abs() < 1e-9,
        "the lease is charged the rise the lender measured, not nothing: {spent}"
    );
}
