//! The exit lock (decisions row 15): an account pinned to a Mac leaves through
//! it on every request, and a Codex account is never carried.

use super::*;

// ---------------------------------------------------------------------------
// The exit lock: one account, one way out, every request (decisions row 15)
// ---------------------------------------------------------------------------

/// [`fake_account`], locked to leave from `peer`.
///
/// The two keys are written the way an operator writes them, as JSON on the
/// account row, so this fixture exercises the parse and not a Rust value that
/// skipped it.
fn pinned_account(peer: &PeerId, strict: bool) -> Account {
    Account {
        egress: tcr::base::config::Egress::Via(*peer),
        egress_strict: strict,
        ..fake_account()
    }
}

/// This Mac's own socket, as a test can see it: a plain HTTP server that counts
/// every connection it accepts and answers the canned body.
///
/// The counter is the whole point. "The pin was honoured" is not a status code,
/// it is the absence of a connection from this box to the upstream, and a 503
/// alone would look the same whether the direct path ran and failed or never
/// ran at all.
fn local_exit(listener: TcpListener) -> Arc<std::sync::atomic::AtomicUsize> {
    let seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = Arc::clone(&seen);
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut head = Vec::new();
            let mut byte = [0_u8; 1];
            while head.len() < 8192 {
                match stream.read(&mut byte).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => head.push(byte[0]),
                }
                if head.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                 connection: close\r\n\r\n{CANNED_BODY}",
                CANNED_BODY.len()
            );
            if stream.write_all(response.as_bytes()).await.is_err() {
                return;
            }
        }
    });
    seen
}

/// The far end of the carry, counting connections and reading nothing.
///
/// It does not speak TLS, and it does not need to: what this fixture measures
/// is WHICH process opened the connection to the origin, which is the whole of
/// the exit lock. A completed 200 through the production carry cannot be
/// observed in this process at all, and the reason is structural rather than an
/// omission: `send_through` builds its client with `tcr::base::http_client::builder`,
/// which carries the bundled webpki roots, so there is no root store a
/// test can add a fixture CA to and no `SSL_CERT_FILE` that would be read. The
/// body half of a carry is already measured, against a real TLS origin, by
/// `a_carry_delivers_the_canned_messages_body_through_a_gateway`.
fn origin_exit(listener: TcpListener) -> Arc<std::sync::atomic::AtomicUsize> {
    let seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = Arc::clone(&seen);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            drop(stream);
        }
    });
    seen
}

fn count(seen: &Arc<std::sync::atomic::AtomicUsize>) -> usize {
    seen.load(std::sync::atomic::Ordering::SeqCst)
}

/// One `pinned_egress` call with the pieces the proxy hunk hands it.
fn attempt<'a>(
    manager: &'a Arc<Manager>,
    method: &'a axum::http::Method,
    peers: &'a std::path::Path,
) -> egress::PinnedAttempt<'a> {
    egress::PinnedAttempt {
        manager,
        account_idx: 0,
        method,
        path_and_query: "/v1/messages",
        headers: axum::http::HeaderMap::new(),
        body: Some(bytes::Bytes::from_static(
            b"{\"model\":\"claude-opus-4-6\",\"messages\":[]}",
        )),
        peers_path: peers,
        session_key: None,
        session_kind: tcr::records::stats::SessionKind::Fallback,
        wire_session_id: None,
        model: None,
        tool_uses: &[],
        tool_results: &[],
    }
}

/// **The positive control for every assertion below.** An account with no pin
/// leaves from this Mac, exactly as it did before the exit lock existed.
///
/// Without this, "the local exit saw nothing" in the next test would be
/// satisfied just as well by a fixture that can never reach the local exit at
/// all.
///
/// Watched red by making `AccountEgress::parse` answer `Local` for every
/// input: unchanged, still green, which is the point of a control. It is
/// watched red the other way, by pinning the account, in
/// `a_pinned_account_never_takes_the_direct_path`.
#[tokio::test]
async fn an_unpinned_account_leaves_from_this_mac() {
    let exit = loopback().await;
    let addr = exit.local_addr().expect("the local exit address");
    let seen = local_exit(exit);

    let manager = fleet_with(&format!("http://{addr}"), vec![fake_account()]);
    let response = post_messages(Arc::clone(&manager)).await;

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "an unpinned account is served by the direct path"
    );
    assert_eq!(
        count(&seen),
        1,
        "the request left from this Mac, which is what an unpinned account does"
    );
}

/// **A pinned account never takes the direct path, on any request.**
///
/// The account is locked to a Mac and `egressStrict` is on, so the exit lock
/// answers before the direct attempt is ever built. The gate is the local
/// exit's counter: three requests, zero connections from this box. A 503 alone
/// would be the same answer whether the hunk ran or the upstream merely refused
/// us.
///
/// The named refusal is asserted in the same breath, because "it did not leave"
/// and "and the operator was told why" are one promise.
///
/// Watched red by commenting out the whole `pinned_egress` match in
/// `src/proxy.rs` (the hunk immediately below `let Some(token) =
/// manager.access_token(idx)`): the local exit sees 3 connections and the
/// status is 200, failing on the counter.
#[tokio::test]
async fn a_pinned_account_never_takes_the_direct_path() {
    let gateway = node();
    let exit = loopback().await;
    let addr = exit.local_addr().expect("the local exit address");
    let seen = local_exit(exit);

    let manager = fleet_with(
        &format!("http://{addr}"),
        vec![pinned_account(&gateway.id, true)],
    );

    for request in 1..=3 {
        let response = post_messages(Arc::clone(&manager)).await;
        assert_eq!(
            response.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "request {request}: a pin that cannot be honoured on a strict account refuses"
        );
        assert_eq!(
            response
                .headers()
                .get("x-should-retry")
                .and_then(|value| value.to_str().ok()),
            Some("true"),
            "request {request}: the Mac may be back in a moment, so the client is told to retry"
        );
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("the refusal body reads");
        let body = String::from_utf8_lossy(&body);
        assert!(
            body.contains("alice@example.com") && body.contains(&gateway.id.display()),
            "request {request}: the refusal names the account and the Mac it is locked to: \
             {body}"
        );
        assert_eq!(
            count(&seen),
            0,
            "request {request}: a locked account must not have left from this Mac"
        );
    }
}

/// **A pinned request leaves through its Mac, on every request.**
///
/// Three requests, one live gateway, and the origin counts the connections it
/// received: all three arrived through the pinned Mac, and the local exit
/// beside it was never touched. That is the exit lock as decisions row 15
/// states it, "lock where accounts go out from which server", measured at the
/// far end rather than inferred from a log line.
///
/// It drives `pinned_egress` directly rather than `proxy::handle`, for one
/// reason: the fleet's upstream has to be a host the mesh carries
/// (`api.anthropic.com`), and through the full handler a regression in the hunk
/// would put a real request on the real internet. `pinned_egress` has no direct
/// path in it at all, so the worst a break can do here is answer nothing.
///
/// Watched red by making `pinned_egress` treat every account as unpinned (`let
/// Some(peer) = pin.egress.peer()` replaced with `let Some(peer) = None`): the
/// origin's counter stays at 0 and the first assertion names it.
#[tokio::test]
async fn a_pinned_request_leaves_through_its_peer_on_every_request() {
    let dir = scratch("pinned-route");
    let peers = dir.join("tcr-peers.json");
    // The requester's key is the one `pinned_egress` will mint in this scratch
    // directory, so the gateway has to pin THAT id rather than one the test
    // chose: minting it here first is what makes the two agree.
    let requester = tcr::peer::config::id::NodeKey::load_or_mint(&dir)
        .expect("mint this node's keypair in the scratch dir");
    let gateway = node();

    let origin = loopback().await;
    let origin_addr = origin.local_addr().expect("the origin address");
    let arrived = origin_exit(origin);

    let local = loopback().await;
    let local_addr = local.local_addr().expect("the local exit address");
    let locally = local_exit(local);

    let gateway_listener = loopback().await;
    let gateway_addr = gateway_listener.local_addr().expect("the gateway address");
    let gateway_task = gateway_serving(
        gateway_listener,
        gateway.secret,
        vec![row(&requester.id(), "requester", Vec::new(), true)],
        origin_addr,
        1024 * 1024,
        3,
    );

    write_carrying_gateway(&peers, &gateway.id, &gateway_addr.to_string());

    let manager = fleet_with(
        &format!("https://{ORIGIN}"),
        vec![pinned_account(&gateway.id, true)],
    );
    for request in 1..=3 {
        match egress::pinned_egress(attempt(&manager, &axum::http::Method::POST, &peers)).await {
            egress::PinnedEgress::Answered(_) => {}
            egress::PinnedEgress::TakeTheDirectPath { .. } => panic!(
                "request {request}: a locked account must never be told to take the direct path \
                 while its Mac is answering"
            ),
        }
    }

    let outcomes = gateway_task.await.expect("the gateway task joins");
    assert_eq!(
        count(&arrived),
        3,
        "every one of the three requests must have reached the origin through the pinned Mac; \
         the gateway saw {outcomes:?}"
    );
    assert_eq!(
        count(&locally),
        0,
        "and none of them may have left from this Mac; the local exit at {local_addr} was \
         never supposed to be touched"
    );
}

/// **A Mac that is not there, on a strict account, refuses by name.**
///
/// The row is pinned and granted the carry and its address is `127.0.0.1:1`,
/// refused by the kernel at once, so the carry is offered and not taken. The
/// assertion is on the words the operator reads: which account, which Mac, and
/// which of the five gaps it was.
///
/// Watched red by changing `Err(err) if pin.strict` to `Err(err) if false` in
/// `pinned_egress`: the answer becomes `TakeTheDirectPath` and the first
/// assertion names it.
#[tokio::test]
async fn a_down_peer_refuses_by_name_when_the_account_is_strict() {
    let dir = scratch("pinned-strict");
    let peers = dir.join("tcr-peers.json");
    let gateway = node();
    write_carrying_gateway(&peers, &gateway.id, "127.0.0.1:1");

    let manager = fleet_with(
        &format!("https://{ORIGIN}"),
        vec![pinned_account(&gateway.id, true)],
    );
    let answer = egress::pinned_egress(attempt(&manager, &axum::http::Method::POST, &peers)).await;
    let egress::PinnedEgress::Answered(response) = answer else {
        panic!("a strict pin that cannot be honoured refuses; it does not fall back");
    };

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok()),
        Some("5"),
        "a Mac that is asleep is a wait, so the client is given one"
    );
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("the refusal body reads");
    let body = String::from_utf8_lossy(&body);
    assert!(
        body.contains("alice@example.com"),
        "the refusal names the account whose row to open: {body}"
    );
    assert!(
        body.contains(&gateway.id.display()),
        "and the Mac it is locked to: {body}"
    );
    assert!(
        body.contains("it did not answer"),
        "and which of the five gaps it was, not a generic failure: {body}"
    );
    assert!(
        !body.contains(&gateway.id.to_wire()),
        "the short display form, never the full pinned key, in anything a client reads: {body}"
    );
}

/// **A carry the gateway TOOK and that then failed is not sent again from this
/// Mac**, even on an account that did not ask to be refused.
///
/// The review's finding: `pinned_egress`'s non-strict arm answered
/// `TakeTheDirectPath` for every error, including the one that means the
/// gateway had already put the request on the wire. The five-second bound also
/// covers the RESPONSE HEAD, so a slow origin reads exactly like a dead one,
/// and the fallback then sent the same POST directly: executed twice, billed
/// twice, on an `egressStrict: false` account whose operator was promised a
/// preference, not a double charge.
///
/// The gateway here is the real one, and the origin behind it accepts the
/// connection and drops it, so the carry is TAKEN and then fails: that is the
/// `CarryFailed` gap and no other. The positive control is the test above,
/// whose gateway is at `127.0.0.1:1` and never takes the carry at all: that one
/// still falls back, which is what it should do.
///
/// Watched red by deleting the `Err(err) if err.request_may_have_left()` arm
/// from `pinned_egress`: the answer becomes `TakeTheDirectPath`.
#[tokio::test]
async fn a_carry_that_was_taken_and_failed_is_not_sent_again_from_this_mac() {
    let dir = scratch("pinned-delivered");
    let peers = dir.join("tcr-peers.json");
    let requester = tcr::peer::config::id::NodeKey::load_or_mint(&dir)
        .expect("mint this node's keypair in the scratch dir");
    let gateway = node();

    // An origin that accepts and drops: the request crosses the splice and the
    // exchange fails afterwards, which is the whole condition.
    let origin = loopback().await;
    let origin_addr = origin.local_addr().expect("the origin address");
    let arrived = origin_exit(origin);

    let gateway_listener = loopback().await;
    let gateway_addr = gateway_listener.local_addr().expect("the gateway address");
    let gateway_task = gateway_serving(
        gateway_listener,
        gateway.secret,
        vec![row(&requester.id(), "requester", Vec::new(), true)],
        origin_addr,
        1024 * 1024,
        1,
    );

    write_carrying_gateway(&peers, &gateway.id, &gateway_addr.to_string());

    let manager = fleet_with(
        &format!("https://{ORIGIN}"),
        vec![pinned_account(&gateway.id, false)],
    );
    let answer = egress::pinned_egress(attempt(&manager, &axum::http::Method::POST, &peers)).await;
    let outcomes = gateway_task.await.expect("the gateway task joins");
    assert_eq!(
        count(&arrived),
        1,
        "the gateway took the carry and the request reached the origin: {outcomes:?}"
    );

    let egress::PinnedEgress::Answered(response) = answer else {
        panic!(
            "a carry that was taken and then failed must be answered, never re-sent from \
             this Mac: {outcomes:?}"
        );
    };
    assert_eq!(
        response.status().as_u16(),
        502,
        "the client is told the outcome is unknown, not handed a retryable 503"
    );
    assert_eq!(
        response
            .headers()
            .get("x-should-retry")
            .and_then(|value| value.to_str().ok()),
        Some("false"),
        "a client that retried this would pay for the request twice"
    );
}

/// **The same Mac, the same outage, on an account that did not ask to be
/// refused: the local path, and exactly one line saying so.**
///
/// One line, not zero and not one per layer: the operator asked for an address
/// and is not getting it, which is worth saying once per request.
///
/// Watched red by deleting the `tracing::warn!` in `pinned_egress`'s non-strict
/// arm: the count is 0 and the second assertion names it.
#[tokio::test]
async fn a_down_peer_falls_back_to_this_mac_with_one_line_when_it_is_not_strict() {
    let dir = scratch("pinned-loose");
    let peers = dir.join("tcr-peers.json");
    let gateway = node();
    write_carrying_gateway(&peers, &gateway.id, "127.0.0.1:1");

    let (collector, _guard) = capture();
    let manager = fleet_with(
        &format!("https://{ORIGIN}"),
        vec![pinned_account(&gateway.id, false)],
    );
    let answer = egress::pinned_egress(attempt(&manager, &axum::http::Method::POST, &peers)).await;
    assert!(
        matches!(
            answer,
            egress::PinnedEgress::TakeTheDirectPath { paced: true }
        ),
        "without `egressStrict` a pin that cannot be honoured is a preference, not a refusal,          and it says the pacing slot is already spent: this request waited on the account's          own bucket inside the exit lock, and `src/proxy.rs` reads this flag rather than          waiting on it a second time"
    );

    let lines = collector.matching("locked to leave from a peer that did not carry it");
    assert_eq!(
        lines.len(),
        1,
        "exactly one line, and it says which account and which Mac: {:?}",
        collector
            .events()
            .iter()
            .map(|event| event.message.clone())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        lines[0].fields.get("account").map(String::as_str),
        Some("alice@example.com")
    );
    assert_eq!(
        lines[0].fields.get("peer").map(String::as_str),
        Some(gateway.id.display().as_str())
    );
}

/// **A pin this build cannot read fails the config LOAD, on every account,
/// strict or not.**
///
/// It used to be refused one request at a time, because the value was parsed
/// out of a stringly map on the request path. It is a declared field now, so
/// serde is what refuses it, and the operator finds out when the file is read
/// rather than once per request. Either way the thing that must never happen is
/// the same: a value the operator typed wrong read as `local` would send the
/// account out of exactly the address it was written down to avoid.
///
/// The short display form is the case worth naming. `PeerId::parse` refuses it,
/// so a pin copied out of a panel row rather than out of `tcr peer ls` is an
/// error and not a prefix match on somebody's key.
///
/// Watched red by making `Egress::try_from` answer `Ok(Self::Local)` for
/// anything it does not recognise: all three loads succeed and the first
/// assertion names it.
#[test]
fn a_pin_that_is_neither_spelling_fails_the_config_load() {
    let account = |egress: &str| {
        format!(
            r#"{{"upstream":"https://api.anthropic.com","accounts":[{{"name":"alice@example.com",
             "accessToken":"at-alice","egress":"{egress}"}}]}}"#
        )
    };

    for wrong in [
        // The display form: short, lossy, and deliberately not parseable back.
        "tcr-7f3k9m2q4x",
        // A full wire id with no `via ` in front of it.
        "0000000000000000000000000000000000000000000000000000",
        // A word that is not the word.
        "remote",
        // The right prefix over the wrong payload.
        "via not-a-peer-id",
        // A colon where the documented form has a space: near enough to be
        // typed by hand, and refused, because two spellings for one thing is
        // how a config stops meaning one thing.
        "via:0000000000000000000000000000000000000000000000000000",
    ] {
        let err = serde_json::from_str::<Config>(&account(wrong))
            .expect_err("a pin that is neither spelling must fail the load, not read as local");
        assert!(
            err.to_string().contains("egress"),
            "and the error must name the key the operator has to fix: {err}"
        );
    }

    let peer = node().id;
    let good = serde_json::from_str::<Config>(&account(&format!("via {}", peer.to_wire())))
        .expect("the `via ` spelling is the one that loads");
    assert_eq!(
        good.accounts[0].egress,
        tcr::base::config::Egress::Via(peer)
    );
    assert!(
        !good.accounts[0].egress_strict,
        "an absent `egressStrict` is not strict"
    );
}

/// The two fields as an operator writes them, read back as one typed value.
///
/// A round trip, not a parse: what is written is what `tcr` writes back, which
/// is the property an operator who edits this file by hand depends on.
///
/// Watched red by making `Egress`'s `Display` print the bare wire id without
/// its `via ` prefix: the round trip comes back as a load error and the last
/// assertion names it.
#[test]
fn an_account_round_trips_the_exit_lock_it_was_written_with() {
    let peer = node().id;

    let plain = fake_account();
    assert_eq!(plain.egress_pin(), tcr::base::config::EgressPin::default());
    assert!(plain.egress_pin().egress.is_local());

    let pinned = pinned_account(&peer, true);
    let pin = pinned.egress_pin();
    assert_eq!(pin.egress.peer(), Some(peer));
    assert!(pin.strict);

    let written = serde_json::to_string(&pinned).expect("an account serializes");
    assert!(
        written.contains(&format!(r#""egress":"via {}""#, peer.to_wire())),
        "the pin is written as one prefixed word: {written}"
    );
    assert!(
        written.contains(r#""egressStrict":true"#),
        "and the strictness beside it, under the name the file uses: {written}"
    );

    let read: Account = serde_json::from_str(&written).expect("and it reads back");
    assert_eq!(read.egress_pin(), pin, "what tcr writes, tcr reads");
}

/// **An unpinned account writes no exit-lock keys at all.**
///
/// `tcr` rewrites the whole config on every save, so a field that serializes
/// its own default stamps that default onto every account row the first time
/// an unrelated setting changes. Two keys times every account, on a file a
/// human reads and edits by hand, for a feature nobody on those rows turned
/// on.
///
/// The pinned half in the same test is the positive control: skipping is a
/// property of the DEFAULT and not of the field, so an assertion that only saw
/// the absence would also pass against a field that never serializes.
///
/// Watched red by dropping `skip_serializing_if` from either field: the plain
/// account's JSON carries `"egress":"local"` or `"egressStrict":false` and the
/// matching assertion names it.
#[test]
fn an_unpinned_account_writes_no_egress_keys() {
    let plain = serde_json::to_string(&fake_account()).expect("an account serializes");
    assert!(
        !plain.contains("egress"),
        "an account nobody pinned writes neither key, so an upgrade does not stamp a \
         default onto every row of a hand-edited file: {plain}"
    );

    let peer = node().id;
    let pinned =
        serde_json::to_string(&pinned_account(&peer, true)).expect("an account serializes");
    assert!(
        pinned.contains(&format!(r#""egress":"via {}""#, peer.to_wire()))
            && pinned.contains(r#""egressStrict":true"#),
        "the control: a row that IS pinned still writes both keys, or the skip above is \
         a field that never serializes: {pinned}"
    );

    let back: Account = serde_json::from_str(&plain).expect("the keyless form reads back");
    assert_eq!(
        back.egress_pin(),
        tcr::base::config::EgressPin::default(),
        "and absent reads as the default it stood for, which is the whole of the round trip"
    );
}

/// **The dry-fleet terminal's doc may not say the fallback provider is absent
/// on this branch.** It is installed at boot and the arm is live.
///
/// The comment read "inert until a phase returns a provider from
/// `fallback::configured_provider`, which is `None` today, so this is
/// byte-for-byte `exhausted_response` on this branch", while
/// `server.rs` installs one from the peers file and then `expect`s it back.
/// A reader who believes that sentence concludes that nothing downstream of the
/// terminal can run, which is how a live borrow loop gets reasoned about as
/// dead code.
///
/// A doc gate rather than a behaviour one because the behaviour is already
/// covered (`tests/peer_lease.rs` drives a real borrow through this arm); what
/// was wrong was only the sentence, and the failure worth catching is that
/// sentence coming back.
///
/// Watched red: restore the "which is `None` today" clause to the doc block
/// above `exhausted_or_fallback` in `src/proxy.rs`.
#[test]
fn the_dry_fleet_terminals_doc_does_not_call_the_fallback_arm_dead() {
    let source = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/proxy/mod.rs"))
        .expect("the proxy's source is readable from the workspace root");
    let lines: Vec<&str> = source.lines().collect();

    // The doc block is the run of `///` lines immediately above the function,
    // walked upwards from the signature. Anchored on the signature rather than
    // on a line number, and the `expect` below is the positive control: a
    // renamed or moved function fails this test loudly instead of scanning an
    // empty span and passing.
    let signature = lines
        .iter()
        .position(|line| line.starts_with("async fn exhausted_or_fallback("))
        .expect(
            "positive control failed: no `exhausted_or_fallback` in src/proxy/mod.rs, so the doc \
             block below is measuring nothing. If the dry-fleet terminal was renamed, rename \
             it here too.",
        );
    let mut first = signature;
    while first > 0 && lines[first - 1].trim_start().starts_with("///") {
        first -= 1;
    }
    let doc = lines[first..signature].join("\n").to_lowercase();
    assert!(
        doc.contains("configured_provider"),
        "positive control failed: the terminal's doc block no longer names the call it is \
         about, so this gate is reading the wrong span: {doc}"
    );

    for claim in [
        "which is `none` today",
        "is `none` today",
        "inert until",
        "byte-for-byte",
    ] {
        assert!(
            !doc.contains(claim),
            "the dry-fleet terminal's doc claims the fallback arm cannot run ({claim}), but \
             the boot sequence installs a provider from the peers file and `server.rs` then \
             expects it back: {doc}"
        );
    }
}

/// Local interception and LAN egress have different scopes. Codex is local only;
/// peer gateways keep the two Anthropic origins they carried before it was added.
#[test]
fn peer_egress_keeps_anthropic_hosts_and_never_carries_chatgpt() {
    assert_eq!(
        egress::PEER_EGRESS_HOSTS,
        &["api.anthropic.com", "platform.claude.com"]
    );
    assert!(egress::host_allowed("api.anthropic.com", 443));
    assert!(egress::host_allowed("platform.claude.com", 443));
    assert!(!egress::host_allowed("chatgpt.com", 443));
    assert!(!egress::host_allowed("CHATGPT.COM", 443));
}

/// **A Codex account is never carried, pinned or not, in this version** (Codex
/// stays off the mesh). `pinned_egress` builds its URL
/// from the ANTHROPIC upstream and splices an Anthropic body, so a pinned Codex
/// account's request went through its Mac to `api.anthropic.com` wearing a
/// ChatGPT bearer. Its pin is one this Mac cannot honour, answered exactly like
/// one: refused by name on a strict account, one warn line and the direct path
/// on a loose one, and the pacing slot left unspent for the direct path to take.
///
/// The gateway here is live and grants the carry, so the origin's counter is
/// the proof: a carry that happened would show up there.
///
/// Watched red by deleting the Codex arm in `pinned_egress`: both requests are
/// carried to the origin through the gateway.
#[tokio::test]
async fn codex_pinned_account_is_never_carried() {
    let dir = scratch("pinned-codex");
    let peers = dir.join("tcr-peers.json");
    let requester = tcr::peer::config::id::NodeKey::load_or_mint(&dir)
        .expect("mint this node's keypair in the scratch dir");
    let gateway = node();

    let origin = loopback().await;
    let origin_addr = origin.local_addr().expect("the origin address");
    let arrived = origin_exit(origin);

    let gateway_listener = loopback().await;
    let gateway_addr = gateway_listener.local_addr().expect("the gateway address");
    let gateway_task = gateway_serving(
        gateway_listener,
        gateway.secret,
        vec![row(&requester.id(), "requester", Vec::new(), true)],
        origin_addr,
        1024 * 1024,
        2,
    );

    write_carrying_gateway(&peers, &gateway.id, &gateway_addr.to_string());

    let (collector, _guard) = capture();
    for strict in [true, false] {
        let manager = fleet_with(
            &format!("https://{ORIGIN}"),
            vec![Account {
                provider: tcr::base::provider::Provider::Codex,
                ..pinned_account(&gateway.id, strict)
            }],
        );
        let answer = egress::pinned_egress(egress::PinnedAttempt {
            path_and_query: tcr::base::provider::CODEX_RESPONSES_PATH,
            ..attempt(&manager, &axum::http::Method::POST, &peers)
        })
        .await;
        match (strict, answer) {
            (true, egress::PinnedEgress::Answered(response)) => {
                assert_eq!(response.status(), StatusCode::BAD_REQUEST);
                assert_eq!(response.headers()["x-should-retry"], "false");
                assert!(!response.headers().contains_key("retry-after"));
                let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
                    .await
                    .expect("the refusal body reads");
                let body = String::from_utf8_lossy(&body);
                let json: serde_json::Value = serde_json::from_str(&body).unwrap();
                assert!(
                    json.get("type").is_none(),
                    "the refusal uses Codex's envelope"
                );
                assert_eq!(json["error"]["code"], "pool_unavailable");
                assert!(
                    body.contains("alice@example.com") && body.contains("Codex"),
                    "the refusal names the account and why its pin cannot be honoured: {body}"
                );
            }
            (false, egress::PinnedEgress::TakeTheDirectPath { paced }) => assert!(
                !paced,
                "nothing was paced inside the exit lock, so the direct path takes the slot"
            ),
            (strict, _) => panic!(
                "a Codex account's pin is refused when strict and falls back when loose \
                 (strict: {strict}); it is never carried"
            ),
        }
    }
    assert_eq!(
        count(&arrived),
        0,
        "a Codex request must never leave through the pinned Mac"
    );
    let lines = collector.matching("does not leave through a peer");
    assert_eq!(
        lines.len(),
        1,
        "the loose account says so exactly once: {:?}",
        collector.messages()
    );
    assert_eq!(
        lines[0].fields.get("account").map(String::as_str),
        Some("alice@example.com")
    );
    gateway_task.abort();
}
