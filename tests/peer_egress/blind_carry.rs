//! The blind carry itself: a real client and TLS session carried through a
//! gateway, what the gateway refuses, the byte cap, the budget of carries still
//! open, and the shipped listener handing a TUNNEL to the production handler.

use super::*;

/// **A requester-side carry that died inside the peer stream says WHY, from the
/// pump that knows.**
///
/// The gateway's own two splices awaited `NoiseStream::finish` and this one
/// dropped its stream, so whatever ended the pump went on the floor. The log
/// line is the whole record of a stream nobody may read, and it said only that
/// the splice failed: not that the frames had stopped decrypting.
///
/// Measured rather than assumed, and the first two versions of this test were
/// each wrong about it. Under tokio 1.53 a pump that dies takes the duplex with
/// it, so the splice ends in an error too and the line is "failed mid-stream"
/// with the pump's ending in its `pump_error` field. Under tokio 1.54 (its io
/// changes keep the shutdown bit and flush on a stalled reader; which one
/// matters here is not measured) the splice ends `Ok` after the local close,
/// and the pump's ending is the `error` field of "the carry ended inside the
/// peer stream". Both lines carry what the awaited `finish()` adds, which is
/// the half no other reader has, so the gate accepts either and pins the
/// field each line owns. Asserting only on the message would have passed
/// without the fix.
///
/// Watch it fail by dropping the `finish()` await in
/// `egress::accept_once_splice`: the line stays, and the pump's field is
/// empty.
#[tokio::test]
async fn a_carry_that_died_inside_the_peer_stream_is_not_logged_as_carried() {
    let (collector, _guard) = capture();
    let requester = node();
    let gateway = node();

    let gateway_listener = loopback().await;
    let gateway_addr = gateway_listener.local_addr().expect("the gateway address");
    let gateway_task = gateway_that_corrupts(
        gateway_listener,
        gateway.secret,
        vec![row(&requester.id, "requester", Vec::new(), true)],
    );

    let splice = egress::accept_once_splice(
        &row(
            &gateway.id,
            "gateway",
            vec![gateway_addr.to_string()],
            false,
        ),
        &requester.secret,
        ORIGIN,
        443,
    )
    .await
    .expect("the gateway takes the carry");

    // One local connection, a byte so the pump has something to fail on, and
    // then CLOSED. The close is load-bearing: it ends the local-to-peer
    // direction cleanly, so the splice itself returns Ok and the only thing
    // that knows the carry died is the pump. Hold the socket open instead and
    // the splice errors on its own, which is an ending the old code reported
    // too, and the gate below would pass without the fix.
    {
        let mut local = TcpStream::connect(("127.0.0.1", splice.port))
            .await
            .expect("dial the loopback splice");
        local.write_all(b"GET / HTTP/1.1\r\n").await.ok();
        local.flush().await.log_debug("flushing a test stream");
        local
            .shutdown()
            .await
            .log_debug("shutting down a test stream");
    }

    // Waited for by its own log line rather than by joining the task: the
    // handle is private on purpose (`PeerSplice::task`, so a streamed response
    // can outlive the function that started it), and widening a production
    // type for a test's convenience is the wrong trade.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let ending = loop {
        let events = collector.events();
        if let Some(event) = events.iter().find(|event| {
            event
                .message
                .contains("the carry ended inside the peer stream")
                || event.message.contains("the carry failed mid-stream")
                || event.message == "peer egress: carried"
        }) {
            break event.clone();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the splice logged no ending at all within 5s: {:?}",
            collector.messages()
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    };
    gateway_task
        .await
        .log_debug("joining the test gateway")
        .and_then(|served| served.log_debug("serving the test gateway"));

    assert_ne!(
        ending.message, "peer egress: carried",
        "a carry whose frames stopped decrypting is not a carry: {ending:?}"
    );
    // Which field holds the pump's ending depends on how the splice ended
    // (see the doc-comment): `pump_error` beside a splice error, `error` when
    // the splice itself was clean and the pump is the only thing that died.
    let field = if ending.message.contains("the carry failed mid-stream") {
        "pump_error"
    } else {
        "error"
    };
    let pump = ending
        .fields
        .get(field)
        .map_or_default(|value| value.trim_matches('"').to_string());
    assert!(
        pump.contains("did not decrypt"),
        "the line carries the pump's own ending in `{field}`, which is the only place a \
         frame that failed to decrypt is visible at all: {ending:?}"
    );
}

/// What a pooled account's answer carries that the local client must never see,
/// and what it carries that must survive.
///
/// The stripped set is `src/proxy.rs`'s own: the whole `anthropic-ratelimit-`
/// family (matched by prefix, so a member Anthropic has not invented yet is
/// covered) plus `anthropic-organization-id`. `request-id` and
/// `anthropic-organization` are the near-misses that prove the filter is not a
/// substring match.
const CARRIED_RESPONSE_HEAD: &str = concat!(
    "HTTP/1.1 200 OK\r\n",
    "content-type: application/json\r\n",
    // REPEATED deliberately: a header build that replaces rather than appends
    // hands the client the last value only. See the cookie assertion below.
    "set-cookie: first=1\r\n",
    "set-cookie: second=2\r\n",
    "content-length: 2\r\n",
    "request-id: req_carried\r\n",
    "anthropic-organization: not-the-id\r\n",
    "anthropic-ratelimit-unified-5h-utilization: 1.0\r\n",
    "anthropic-ratelimit-requests-remaining: 0\r\n",
    "anthropic-ratelimit-something-new: 1\r\n",
    "anthropic-organization-id: 11111111-1111-1111-1111-111111111111\r\n",
    "\r\n",
    "{}"
);

/// **A carried response is filtered by the proxy's own rules, not by a local
/// list.**
///
/// The review's finding at `egress.rs:906`: `axum_response_from` kept a
/// private eight-name hop-by-hop list: two names short of
/// `proxy::is_response_skip`, and with no equivalent of
/// `proxy::is_account_scoped` at all: so a carried response handed the local
/// client the pooled account's quota window and its org id, the two things the
/// direct path strips because rotation means they describe a different account
/// on every request. Claude Code renders its usage banner off exactly those
/// headers.
///
/// Driven against a plain loopback origin rather than through
/// `retry_through_peer`, because a carry that completes end to end resolves
/// `api.anthropic.com` for real (the allow-list), which this suite may not do.
/// The response handed to the function is a real `reqwest::Response` with real
/// headers off a real socket.
///
/// Watched red by restoring the old `is_hop_by_hop` filter in
/// `axum_response_from`: `anthropic-ratelimit-unified-5h-utilization` reaches
/// the client and the assertion below names it.
#[tokio::test]
async fn a_carried_response_never_hands_the_client_the_serving_accounts_headers() {
    let listener = loopback().await;
    let addr = listener.local_addr().expect("the origin address");
    let origin = tokio::spawn(async move {
        let (mut stream, _peer) = listener.accept().await.expect("one connection");
        let mut buf = [0_u8; 2048];
        let head = stream.read(&mut buf).await.expect("the request head");
        assert!(head > 0, "the client must send a request before an answer");
        stream
            .write_all(CARRIED_RESPONSE_HEAD.as_bytes())
            .await
            .expect("the canned answer");
        stream.flush().await.expect("flush");
    });

    let response = tcr::http_client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .expect("a client")
        .get(format!("http://{addr}/v1/messages"))
        .send()
        .await
        .expect("the loopback origin answers");
    let carried = egress::axum_response_from(response);
    origin.await.expect("the origin task joins");

    assert_eq!(carried.status(), StatusCode::OK);
    for name in [
        "anthropic-ratelimit-unified-5h-utilization",
        "anthropic-ratelimit-requests-remaining",
        "anthropic-ratelimit-something-new",
        "anthropic-organization-id",
    ] {
        assert!(
            carried.headers().get(name).is_none(),
            "{name} describes the pooled account that carried this, not the caller"
        );
    }
    assert!(
        carried.headers().get("content-length").is_none(),
        "the framing headers are the carried connection's, and the body is re-streamed"
    );
    assert_eq!(
        carried
            .headers()
            .get("request-id")
            .map(|value| value.to_str().expect("ascii")),
        Some("req_carried"),
        "a request id identifies a REQUEST and is what makes one failed call debuggable"
    );
    assert_eq!(
        carried
            .headers()
            .get("anthropic-organization")
            .map(|value| value.to_str().expect("ascii")),
        Some("not-the-id"),
        "the family is a prefix match, not a substring one"
    );
    // **A REPEATED HEADER KEEPS EVERY VALUE.** The header build used `insert`,
    // which REPLACES, while the direct path uses `append`: a response that
    // legitimately repeats a name (`set-cookie` every day, `warning` and
    // `link` besides) reached the client with its last value only, so a
    // carried answer quietly carried less than a directly-served one.
    //
    // Watched red: put `insert` back and only `second=2` arrives.
    let cookies: Vec<&str> = carried
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|value| value.to_str().expect("ascii"))
        .collect();
    assert_eq!(
        cookies,
        ["first=1", "second=2"],
        "a carried response keeps every value of a repeated header, the way the direct \
         path does"
    );
    assert_eq!(
        carried
            .headers()
            .get("content-type")
            .map(|value| value.to_str().expect("ascii")),
        Some("application/json")
    );
}

// ---------------------------------------------------------------------------
// Item 1: the tunnel carries a real TLS session, blind
// ---------------------------------------------------------------------------

/// The whole carry: a real client, a real TLS handshake, a real HTTP answer,
/// through a gateway that never held a key for any of it.
///
/// The value assertion is the canned body, not merely a 200: a splice that
/// crossed its two directions, or dropped the last frame, still answers with
/// SOMETHING. It also asserts the gateway's own byte counters are non-zero in
/// both directions, because a carry that only ever went one way would be a
/// half-working splice with a passing status line.
#[tokio::test]
async fn a_carry_delivers_the_canned_messages_body_through_a_gateway() {
    let (collector, _guard) = capture();
    let requester = node();
    let gateway = node();
    let origin = mint_origin();

    let origin_listener = loopback().await;
    let origin_addr = origin_listener.local_addr().expect("the origin address");
    let origin_task = fake_origin_on(origin_listener, &origin);

    let gateway_listener = loopback().await;
    let gateway_addr = gateway_listener.local_addr().expect("the gateway address");
    let gateway_task = gateway_on(
        gateway_listener,
        gateway.secret,
        vec![row(&requester.id, "requester", Vec::new(), true)],
        OriginRoute::Fixed(origin_addr),
        1024 * 1024,
    );

    let splice = egress::accept_once_splice(
        &row(
            &gateway.id,
            "gateway",
            vec![gateway_addr.to_string()],
            false,
        ),
        &requester.secret,
        ORIGIN,
        443,
    )
    .await
    .expect("the gateway takes the carry");

    let response = carried_client(&origin, splice.port)
        .post(format!("https://{ORIGIN}/v1/messages"))
        .header("content-type", "application/json")
        .body(r#"{"model":"claude-opus-4-6","messages":[]}"#)
        .send()
        .await
        .expect("the carried request completes");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.expect("the carried body");
    assert_eq!(
        body, CANNED_BODY,
        "the body the origin sent must be the body the client read, byte for byte"
    );

    let head = origin_task
        .await
        .expect("the origin task joins")
        .expect("the origin served");
    assert!(
        head.starts_with("POST /v1/messages "),
        "the origin must see the request the client made, not a rewritten one: {head:?}"
    );

    let (up, down) = gateway_task
        .await
        .expect("the gateway task joins")
        .expect("the gateway carried the stream");
    assert!(up > 0, "the gateway must have carried bytes outbound");
    assert!(down > 0, "the gateway must have carried bytes inbound");

    // **The gateway's log line, field for field.** A gateway is the one place
    // where a lazy log line would leak a peer's whole request stream, so the
    // assertion is on the exact field SET and not on a few fields being
    // present: anything added later: a path, a header, a body length, a
    // request id: fails here.
    let carried = collector.matching("peer tunnel: carried");
    assert_eq!(carried.len(), 1, "one line per carried stream: {carried:?}");
    let mut keys: Vec<&str> = carried[0].fields.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec!["bytes_down", "bytes_up", "host", "ms", "peer", "port"],
        "the gateway may log the peer, the host, the port, two byte counts and a duration, \
         and nothing else"
    );
    assert_eq!(
        carried[0].fields.get("host").map(String::as_str),
        Some(ORIGIN)
    );
    // The peer is the SHORT display form, never the full key: a log file is
    // not where a pinned static key belongs.
    assert_eq!(
        carried[0].fields.get("peer").map(String::as_str),
        Some(requester.id.display().as_str())
    );
    assert!(
        !carried[0]
            .fields
            .values()
            .any(|value| value.contains(&requester.id.to_wire()))
    );
}

/// A gateway refuses a host that is not on the allow-list, and refuses it
/// BEFORE anything is dialled.
///
/// Deny by default is the inverse of the local proxy's blind-tunnel default,
/// and this is the test the module docs promise.
#[tokio::test]
async fn a_host_off_the_allow_list_is_refused() {
    let requester = node();
    let gateway = node();
    let gateway_listener = loopback().await;
    let gateway_addr = gateway_listener.local_addr().expect("the gateway address");
    let rows = vec![row(&requester.id, "requester", Vec::new(), true)];
    let gateway_task = gateway_on(
        gateway_listener,
        gateway.secret,
        rows,
        // A fixed route that would succeed if the allow-list did not run
        // first: the refusal must not be an accident of an unreachable host.
        OriginRoute::Fixed(SocketAddr::from(([127, 0, 0, 1], 9))),
        1024 * 1024,
    );

    // Held to the end of the test, so the gateway's refusal is its own and not
    // a reaction to the requester hanging up.
    let (_stream, _session) = dial_and_open(
        gateway_addr,
        &requester,
        &gateway.id,
        &tunnel_to_origin("example.com", 7),
    )
    .await;

    let refusal = gateway_task
        .await
        .expect("the gateway task joins")
        .expect_err("a host off the allow-list must be refused");
    let text = format!("{refusal:#}");
    assert!(
        text.contains("example.com") && text.contains("allow-list"),
        "the refusal must name the host and the reason: {text}"
    );
}

/// A gateway grant is not a relay grant, at both checks that exist.
///
/// The production gate refuses `TunnelTarget::Peer` for want of `allow.relay`,
/// and the GATEWAY handler refuses it again even when a grant admits it ,
/// because `handle_tunnel_on` is handed no peers file and so can check neither
/// the relay grant nor the target's pin. Two checks, measured separately,
/// because a reader who saw only one would not know whether an `allow.relay`
/// grant opens an unbounded relay through the gateway path.
///
/// Forwarding itself exists now and is `tunnel::handle_forward_on`, gated by
/// `tests/peer_forward.rs`. What this test keeps measuring is that the two
/// entry points stayed apart: a carry never becomes a forward by a grant.
#[tokio::test]
async fn a_relay_target_is_refused_by_a_gateway() {
    let requester = node();
    let gateway = node();
    let onward = node();
    let header = StreamHeader {
        kind: StreamKind::Tunnel,
        target: Some(TunnelTarget::Peer { node: onward.id }),
        via: Vec::new(),
        hops_remaining: 1,
        request_id: 8,
    };

    // Check one: the gate, under a gateway-only grant.
    let gateway_only = row(&requester.id, "requester", Vec::new(), true);
    let refusal = listener::peer_stream_gate_rows(&header, Some(&gateway_only))
        .expect_err("a relay target under a gateway grant must be refused");
    assert!(
        format!("{refusal}").contains("relay") || format!("{refusal:?}").contains("NotGranted"),
        "the gate's refusal must be about the missing grant: {refusal:?}"
    );
    // And the same gate admits the origin form, so the refusal above is about
    // the TARGET and not about the kind.
    let origin_header = StreamHeader {
        target: Some(TunnelTarget::Origin {
            host: ORIGIN.to_string(),
            port: 443,
        }),
        ..header.clone()
    };
    assert!(listener::peer_stream_gate_rows(&origin_header, Some(&gateway_only)).is_ok());

    // Check two: the handler, given a relay target with the gate out of the
    // way. `relay` is granted on the row here so the gate cannot be what
    // refuses, and the header on the wire is the origin form because the wire
    // cannot carry the other one (see `gateway_on_target`).
    let mut relaying = row(&requester.id, "requester", Vec::new(), true);
    relaying.allow.relay = true;
    let gateway_listener = loopback().await;
    let gateway_addr = gateway_listener.local_addr().expect("the gateway address");
    let gateway_task = gateway_on_target(
        gateway_listener,
        gateway.secret,
        vec![relaying],
        OriginRoute::Resolve,
        1024 * 1024,
        Some(TunnelTarget::Peer { node: onward.id }),
    );

    let (_stream, _session) =
        dial_and_open(gateway_addr, &requester, &gateway.id, &origin_header).await;

    let refusal = gateway_task
        .await
        .expect("the gateway task joins")
        .expect_err("the handler must refuse a relay target too");
    let text = format!("{refusal:#}");
    assert!(
        text.contains("relay"),
        "the handler's refusal must say it does not relay: {text}"
    );
}

/// A relay target round-trips on the wire, which is what forward-dial
/// had to land before a client could ask for one at all.
///
/// # This test is the INVERSE of the one it replaces
///
/// It was `a_relay_target_cannot_be_serialized_yet`, and it asserted that
/// `serde_json::to_vec` REFUSED this header: `TunnelTarget` is internally
/// tagged (`tag = "target"`), `PeerId` serializes as a string, and serde
/// refuses an internally-tagged newtype variant whose content is not a map. So
/// `TUNNEL{Peer}` could not be written by any node, and the forwarder every
/// gate in `tests/peer_forward.rs` measures was reachable by nobody. That
/// test's own failure message said what to do when the patch landed, and this
/// is it: the variant is now `Peer { node }` and the assertion is that it
/// travels, and travels back as the same id.
#[test]
fn a_relay_target_travels_on_the_wire() {
    let header = StreamHeader {
        kind: StreamKind::Tunnel,
        target: Some(TunnelTarget::Peer {
            node: PeerId([6_u8; 32]),
        }),
        via: Vec::new(),
        hops_remaining: 1,
        request_id: 9,
    };
    let bytes = serde_json::to_vec(&header).expect("a relay target serializes now");
    let read: StreamHeader = serde_json::from_slice(&bytes).expect("and reads back");
    assert_eq!(
        read.target,
        Some(TunnelTarget::Peer {
            node: PeerId([6_u8; 32])
        }),
        "the forwarder learns WHICH pinned Mac to carry to from this field and nothing \
         else, so a round trip that lost it would be an unroutable forward"
    );
}

/// A ClientHello that names another host closes the carry, and the gateway
/// never dials the origin.
#[tokio::test]
async fn a_mismatched_sni_closes_the_carry() {
    let requester = node();
    let gateway = node();
    let gateway_listener = loopback().await;
    let gateway_addr = gateway_listener.local_addr().expect("the gateway address");
    let gateway_task = gateway_on(
        gateway_listener,
        gateway.secret,
        vec![row(&requester.id, "requester", Vec::new(), true)],
        OriginRoute::Fixed(SocketAddr::from(([127, 0, 0, 1], 9))),
        1024 * 1024,
    );

    let splice = egress::accept_once_splice(
        &row(
            &gateway.id,
            "gateway",
            vec![gateway_addr.to_string()],
            false,
        ),
        &requester.secret,
        ORIGIN,
        443,
    )
    .await
    .expect("the gateway takes the carry");

    // A real ClientHello for the WRONG name, produced by a real client: the
    // point is a handshake this gateway was not asked to carry, not a
    // hand-written byte string only this parser would accept.
    let wrong = tcr::http_client::builder()
        .no_proxy()
        .resolve(
            "platform.claude.com",
            SocketAddr::from(([127, 0, 0, 1], splice.port)),
        )
        .build()
        .expect("a client");
    let attempt = wrong.get("https://platform.claude.com/").send().await;
    assert!(
        attempt.is_err(),
        "a domain-fronted carry must fail at the client too, not merely be logged"
    );

    let refusal = gateway_task
        .await
        .expect("the gateway task joins")
        .expect_err("a mismatched SNI must be refused");
    let text = format!("{refusal:#}");
    assert!(
        text.contains("platform.claude.com") && text.contains(ORIGIN),
        "the refusal must name both the host asked for and the host named: {text}"
    );
}

/// A peer that opens a carry and then says nothing is closed on, rather than
/// holding a task on somebody else's Mac for as long as it likes.
///
/// Found by a mutation: with the origin allow-list disabled, the refusal test
/// stopped refusing and hung instead, which said the deadline was missing
/// rather than that the mutation was wrong.
#[tokio::test]
async fn a_carry_that_sends_no_client_hello_is_closed_on() {
    let requester = node();
    let gateway = node();
    let gateway_listener = loopback().await;
    let gateway_addr = gateway_listener.local_addr().expect("the gateway address");
    let gateway_task = gateway_on(
        gateway_listener,
        gateway.secret,
        vec![row(&requester.id, "requester", Vec::new(), true)],
        OriginRoute::Fixed(SocketAddr::from(([127, 0, 0, 1], 9))),
        1024 * 1024,
    );

    let (_stream, _session) = dial_and_open(
        gateway_addr,
        &requester,
        &gateway.id,
        &tunnel_to_origin(ORIGIN, 11),
    )
    .await;
    // And then nothing at all.

    let started = std::time::Instant::now();
    let refusal = tokio::time::timeout(tunnel::FIRST_RECORD_TIMEOUT * 4, gateway_task)
        .await
        .expect("the gateway must not wait forever")
        .expect("the gateway task joins")
        .expect_err("a silent carry must be closed on");
    assert!(
        format!("{refusal:#}").contains("no complete ClientHello"),
        "the refusal must name what was missing: {refusal:#}"
    );
    assert!(
        started.elapsed() < tunnel::FIRST_RECORD_TIMEOUT * 3,
        "the deadline must be the five-second one, not a socket timeout"
    );
}

/// The parser reads the SNI out of a REAL rustls ClientHello.
///
/// The control that makes every other SNI assertion here worth anything: a
/// hand-built fixture would agree with this parser and with nothing else.
#[tokio::test]
async fn the_sni_parser_reads_a_real_client_hello() {
    let listener = loopback().await;
    let port = listener.local_addr().expect("the address").port();
    let client = tcr::http_client::builder()
        .no_proxy()
        .resolve(ORIGIN, SocketAddr::from(([127, 0, 0, 1], port)))
        .build()
        .expect("a client");
    // Nothing answers, so this fails: after writing a real ClientHello.
    let attempt =
        tokio::spawn(async move { client.get(format!("https://{ORIGIN}/")).send().await });

    let (mut stream, _) = listener.accept().await.expect("accept the handshake");
    let mut first = vec![0_u8; 4096];
    let read = stream.read(&mut first).await.expect("the ClientHello");
    first.truncate(read);
    drop(stream);
    attempt
        .await
        .log_debug("joining the test client")
        .and_then(|sent| sent.log_debug("sending through the closed handshake"));

    assert_eq!(
        tunnel::client_hello_sni(&first)
            .expect("a ClientHello parses")
            .as_deref(),
        Some(ORIGIN),
        "the parser must read the name a real client sent"
    );
    assert!(tunnel::assert_sni_matches_target(&first, ORIGIN).is_ok());
    let mismatch = tunnel::assert_sni_matches_target(&first, "platform.claude.com")
        .expect_err("a mismatch must be refused");
    assert!(format!("{mismatch:#}").contains("closing"));
    // Case is not a mismatch: a DNS name is case-insensitive.
    assert!(tunnel::assert_sni_matches_target(&first, "API.Anthropic.CoM").is_ok());
    // Bytes that are not a handshake record at all are a different fact from a
    // handshake with no name, and get a different message.
    let refusal = tunnel::client_hello_sni(b"GET / HTTP/1.1\r\n\r\n")
        .expect_err("plain HTTP is not a ClientHello");
    assert!(format!("{refusal:#}").contains("not a TLS handshake"));
}

// ---------------------------------------------------------------------------
// Item 3: the byte cap
// ---------------------------------------------------------------------------

/// The ledger rolls its hour, and a spent hour refuses the next carry.
///
/// **The allowance figures moved and the old ones are gone on
/// purpose.** They used to be "whatever is left of the hour", which is what let
/// concurrent carries spend the hour several times over
/// (`five_concurrent_carries_at_a_four_carry_budget_refuse_the_fifth`); one
/// carry now holds one `cap / MAX_OPEN_CARRIES_PER_PEER` slice. The facts this
/// test exists for: the hour is per peer, a spent hour refuses, and the window
/// rolls, are unchanged.
#[test]
fn a_spent_hour_refuses_the_next_carry() {
    let peer = PeerId([3_u8; 32]);
    let other = PeerId([4_u8; 32]);
    let mut budget = TunnelBudget::new();
    let now = 1_767_225_600_000_i64;
    // One slice of a 1000-byte hour, with four open carries allowed.
    let slice = 1000 / tunnel::MAX_OPEN_CARRIES_PER_PEER;
    let open = match budget.admit(&peer, 1000, now) {
        Admission::Carry { allowance, open } => {
            assert_eq!(allowance, slice);
            open
        }
        refused => panic!("an untouched hour must admit a carry, got {refused:?}"),
    };
    // Closed having spent 600, which is more than its slice only because a
    // test may say so; the ledger records what it is told.
    budget.close(open, &peer, 600, now);
    let open = match budget.admit(&peer, 1000, now) {
        Admission::Carry { allowance, open } => {
            assert_eq!(
                allowance,
                400.min(slice),
                "what is left, capped at one slice"
            );
            open
        }
        refused => panic!("400 bytes left must admit a carry, got {refused:?}"),
    };
    budget.close(open, &peer, 400, now);
    assert_eq!(
        budget.admit(&peer, 1000, now),
        Admission::OverBudget {
            spent: 1000,
            cap: 1000
        }
    );
    // The cap is per peer, not per gateway: one peer spending its hour must not
    // refuse another peer's carry.
    assert!(matches!(
        budget.admit(&other, 1000, now),
        Admission::Carry { .. }
    ));
    // And the hour rolls.
    let later = now + tunnel::BUDGET_WINDOW_MS + 1;
    assert!(matches!(
        budget.admit(&peer, 1000, later),
        Admission::Carry { .. }
    ));
    assert_eq!(budget.spent_by(&peer, later), 0);
}

/// A peer whose hour is spent is refused by the gateway itself, with a byte cap
/// small enough that the refusal is the only possible outcome.
#[tokio::test]
async fn a_gateway_refuses_a_carry_over_the_byte_cap() {
    let requester = node();
    let gateway = node();
    let gateway_listener = loopback().await;
    let gateway_addr = gateway_listener.local_addr().expect("the gateway address");
    // Zero bytes for the hour: the refusal happens at admission, before a
    // ClientHello is read and before the origin is dialled.
    let gateway_task = gateway_on(
        gateway_listener,
        gateway.secret,
        vec![row(&requester.id, "requester", Vec::new(), true)],
        OriginRoute::Fixed(SocketAddr::from(([127, 0, 0, 1], 9))),
        0,
    );

    let splice = egress::accept_once_splice(
        &row(
            &gateway.id,
            "gateway",
            vec![gateway_addr.to_string()],
            false,
        ),
        &requester.secret,
        ORIGIN,
        443,
    )
    .await
    .expect("the gateway takes the stream header");
    // Nothing needs to dial the splice: the gateway refuses on the header.
    splice.abort();

    let refusal = gateway_task
        .await
        .expect("the gateway task joins")
        .expect_err("a spent hour must refuse the carry");
    let text = format!("{refusal:#}");
    assert!(
        text.contains("carried bytes this hour"),
        "the refusal must name the cap: {text}"
    );
}

/// The cap is enforced DURING a carry too, not only at admission.
///
/// A cap that only admitted would be a cap on stream count: one stream that
/// never ends could spend any number of bytes. The allowance here is smaller
/// than the canned answer, so the carry starts and then stops.
#[tokio::test]
async fn a_carry_stops_when_its_allowance_runs_out() {
    let requester = node();
    let gateway = node();
    let origin = mint_origin();

    let origin_listener = loopback().await;
    let origin_addr = origin_listener.local_addr().expect("the origin address");
    let _origin_task = fake_origin_on(origin_listener, &origin);

    let gateway_listener = loopback().await;
    let gateway_addr = gateway_listener.local_addr().expect("the gateway address");
    // Enough to be admitted, far too little for a TLS handshake.
    let gateway_task = gateway_on(
        gateway_listener,
        gateway.secret,
        vec![row(&requester.id, "requester", Vec::new(), true)],
        OriginRoute::Fixed(origin_addr),
        64,
    );

    let splice = egress::accept_once_splice(
        &row(
            &gateway.id,
            "gateway",
            vec![gateway_addr.to_string()],
            false,
        ),
        &requester.secret,
        ORIGIN,
        443,
    )
    .await
    .expect("the gateway takes the carry");

    let attempt = carried_client(&origin, splice.port)
        .post(format!("https://{ORIGIN}/v1/messages"))
        .body("{}")
        .send()
        .await;
    assert!(
        attempt.is_err(),
        "a carry cut off at its allowance must fail the request, not answer it"
    );
    let outcome = gateway_task.await.expect("the gateway task joins");
    assert!(
        outcome.is_err(),
        "the gateway must report the carry it cut off: {outcome:?}"
    );
}

// ---------------------------------------------------------------------------
// The cap counts the carries that are still open
// ---------------------------------------------------------------------------

/// **Item 3, red first.** Five carries opened at once against a four-carry
/// budget: the fifth is refused, and closing one lets the next in.
///
/// Before this change the ledger only charged a carry when it CLOSED, so all
/// five were admitted and each was handed the whole remaining hour as its
/// allowance: a per-hour cap that five concurrent streams could spend five
/// times over. Watched red by deleting `self.reserved_by(peer)` from
/// `TunnelBudget::admit`.
#[test]
fn five_concurrent_carries_at_a_four_carry_budget_refuse_the_fifth() {
    let peer = PeerId([51_u8; 32]);
    let other = PeerId([52_u8; 32]);
    let now = 1_767_225_600_000_i64;
    // A cap of exactly four slices, so the arithmetic is the assertion rather
    // than a coincidence of the constant.
    let cap = 4 * 1_000_u64;
    let mut budget = TunnelBudget::new();

    let mut open = Vec::new();
    for n in 0..4 {
        match budget.admit(&peer, cap, now) {
            Admission::Carry {
                allowance,
                open: id,
            } => {
                assert_eq!(allowance, 1_000, "carry {n} holds one slice of the hour");
                open.push(id);
            }
            refused => panic!("carry {n} of four must be admitted, got {refused:?}"),
        }
    }
    assert_eq!(
        budget.reserved_by(&peer),
        cap,
        "four open carries hold the whole hour"
    );
    assert_eq!(
        budget.admit(&peer, cap, now),
        Admission::OverBudget { spent: cap, cap },
        "the fifth is refused while the other four are still open"
    );
    // Another peer's hour is untouched: the cap is per peer.
    assert!(matches!(
        budget.admit(&other, cap, now),
        Admission::Carry { .. }
    ));

    // One closes having spent almost nothing, and its slice comes back.
    let closed = open.pop().expect("four were opened");
    budget.close(closed, &peer, 10, now);
    match budget.admit(&peer, cap, now) {
        Admission::Carry { allowance, .. } => assert_eq!(
            allowance, 990,
            "the slice that came back, less what the closed carry actually spent"
        ),
        refused => panic!("a released slice must admit the next carry, got {refused:?}"),
    }
    // Closing the same reservation twice cannot free a second slice.
    let before = budget.reserved_by(&peer);
    budget.close(closed, &peer, 0, now);
    assert_eq!(
        budget.reserved_by(&peer),
        before,
        "a double close charges, and frees nothing extra"
    );
}

// ---------------------------------------------------------------------------
// The REAL listener carries a TUNNEL
// ---------------------------------------------------------------------------

/// **The shipped dispatch arm hands a TUNNEL to the production handler.**
///
/// Every other carry test in this file drives `gateway_on`, a test-only accept
/// loop, because `listener::serve_stream`'s `StreamKind::Tunnel` arm was a
/// `bail!("phase 6 lands TUNNEL")`: so the three production calls were
/// measured in the right order by a copy of the arm rather than by the arm.
/// This one drives `listener::serve_on_with`: the shipped accept loop, the
/// shipped handshake, the shipped stream gate and the shipped dispatch.
///
/// **What it measures, and why it stays off the network.** The arm carries with
/// `OriginRoute::Resolve`, which in production resolves `api.anthropic.com`
/// for real, so a completed carry cannot be a test on this box. It does not
/// need to be: the handler answers the allow-list and the byte cap and then
/// waits for the requester's ClientHello for `FIRST_RECORD_TIMEOUT` BEFORE it
/// dials anything (`tunnel::handle_tunnel_on`'s documented order). A requester
/// that sends the header and then nothing at all therefore proves the whole
/// arm: the stream reached the handler, which owned it and closed it on its
/// own five-second deadline, without one packet leaving this Mac.
///
/// Watched red: restore the arm's `bail!` and the connection is dropped
/// immediately after the header, so `elapsed` is milliseconds rather than the
/// five seconds asserted below.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_real_listener_hands_a_tunnel_to_the_production_handler() {
    let dir = scratch("real-listener-tunnel");
    let requester = node();
    // A pinned requester holding the one carry grant, in a peers file written
    // by the one writer (0600, atomic).
    let (gateway_key, gateway_addr) = real_gateway_pinning(&dir, &requester).await;
    let (mut stream, _session) = dial_and_open(
        gateway_addr,
        &requester,
        &gateway_key.id(),
        &tunnel_to_origin(ORIGIN, 81),
    )
    .await;

    // And then nothing at all: the handler's own deadline is what closes this.
    let started = std::time::Instant::now();
    let mut carried = Vec::new();
    let closed = tokio::time::timeout(
        tunnel::FIRST_RECORD_TIMEOUT * 3,
        stream.read_to_end(&mut carried),
    )
    .await;
    let elapsed = started.elapsed();

    assert!(
        closed.is_ok(),
        "the production handler must close a silent carry on its own deadline, not hold the \
         socket open: still open after {elapsed:?}"
    );
    assert!(
        elapsed >= tunnel::FIRST_RECORD_TIMEOUT,
        "the stream reached `tunnel::handle_tunnel_on` and was closed by its \
         FIRST_RECORD_TIMEOUT, so the close cannot be quicker than that: {elapsed:?}. A close \
         in milliseconds is the dispatch arm refusing the kind outright."
    );
    assert!(
        elapsed < tunnel::FIRST_RECORD_TIMEOUT * 2,
        "and it is that deadline rather than some socket timeout: {elapsed:?}"
    );
}

/// **A TUNNEL whose header names no target is refused by the shipped arm.**
///
/// The gate above it (`peer_stream_gate_rows`) already refuses
/// `(Tunnel, None)`, so this is the arm's own belt-and-braces: the pattern
/// match that reads the target cannot be written as an `unwrap`, and the
/// refusal has to be the arm's rather than a panic in a connection task.
///
/// Watched with the gate's `(StreamKind::Tunnel, None) => false` row flipped to
/// `true`: the arm still refuses and the listener still answers other
/// connections, which is what this measures.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tunnel_with_no_target_is_refused_by_the_real_listener() {
    let dir = scratch("real-listener-tunnel-no-target");
    let requester = node();
    let (gateway_key, gateway_addr) = real_gateway_pinning(&dir, &requester).await;
    let header = StreamHeader {
        target: None,
        ..tunnel_to_origin(ORIGIN, 82)
    };
    let (mut stream, _session) =
        dial_and_open(gateway_addr, &requester, &gateway_key.id(), &header).await;

    let mut carried = Vec::new();
    let read = tokio::time::timeout(
        tunnel::FIRST_RECORD_TIMEOUT,
        stream.read_to_end(&mut carried),
    )
    .await
    .expect("a targetless TUNNEL is refused at once, not waited on")
    .expect("the socket reads to its end");
    assert_eq!(
        read, 0,
        "the refusal writes nothing back: a stranger's malformed header earns no answer"
    );
}
