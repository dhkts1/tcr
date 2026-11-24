//! The blind carry, end to end and at its edges.
//!
//! # What is real here and what is a stand-in
//!
//! Real: the Noise handshake and session, the stream header, the production
//! authorization gate (`listener::peer_stream_gate_rows`), the production
//! gateway handler (`tunnel::handle_tunnel_on`), the production requester side
//! (`egress::accept_once_splice`), a real rustls client, a real TLS handshake
//! and a real HTTP response carried through the splice.
//!
//! Stand-ins, both named at their call site: the gateway's accept loop, for the
//! reason `gateway_on` gives, and
//! `the_real_listener_hands_a_tunnel_to_the_production_handler` drives it, but
//! it carries with `OriginRoute::Resolve` and a completed carry on this box
//! must not resolve a real name; and the origin's address, because the
//! allow-list is by
//! NAME (`api.anthropic.com`) and a test cannot rewrite the machine's resolver,
//! so the gateway is handed `OriginRoute::Fixed` while production always
//! resolves.
//!
//! # House rules this file is built to
//!
//! Every socket binds `127.0.0.1:0` (kernel-chosen), every file is under a
//! process-and-thread-unique scratch directory, no account is real
//! (`alice@example.com`, `1111…`), and nothing reads the operator's config
//! directory or cache directory: `egress::install_peers_path` points the seam
//! at the scratch copy before any request runs. Nothing here touches the proxy on
//! `127.0.0.1:3456`.

/// The environment every launch of the built `tcr` starts from.
#[path = "support/tcr_env.rs"]
mod tcr_env;

/// The harness pieces the `peer_*` test files share.
#[path = "support/peer.rs"]
mod peer_support;

use peer_support::{Collector, Node, dial_and_open, loopback, node};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tcr::log_err::LogErr as _;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tcr::config::{Account, Config};
use tcr::manager::Manager;
use tcr::oauth::{OAuthError, RefreshFuture, TokenRefresher};
use tcr::peer::config::{Allow, Endpoint, EndpointSource, PeerFile, PeerRow};
use tcr::peer::egress::{self, GatewayCandidate, ViaSetting};
use tcr::peer::listener;
use tcr::peer::noise;
use tcr::peer::tunnel::{self, Admission, Carry, OriginRoute, TunnelBudget};
use tcr::probe::{ProbeError, ProbeFuture, UsageProber};
use tcr::warmer::{AccountWarmer, WarmError, WarmFuture};
use tcr_peer_wire::{PeerId, StreamHeader, StreamKind, TunnelTarget};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tower::ServiceExt as _;

/// The blind carry itself: a real TLS session through a gateway, its refusals,
/// the byte cap, the open-carries budget and the real listener's TUNNEL arm.
#[path = "peer_egress/blind_carry.rs"]
mod blind_carry;

/// `tcr peer via`: the words it accepts, the Mac it resolves to, the stored
/// setting and the operator's timeout.
#[path = "peer_egress/via_setting.rs"]
mod via_setting;

/// The seam into the proxy: when a carry is offered, the metric line, and a
/// carried request counted as a served one.
#[path = "peer_egress/seam_and_guard.rs"]
mod seam_and_guard;

/// The exit lock: one account, one way out, every request.
#[path = "peer_egress/exit_lock.rs"]
mod exit_lock;

/// The one origin this mesh carries, and the name every certificate, SNI check
/// and allow-list decision in this file is about.
const ORIGIN: &str = "api.anthropic.com";

/// What the fake origin answers, so a carried body is recognisable at the far
/// end rather than merely non-empty.
const CANNED_BODY: &str =
    r#"{"id":"msg_carried","type":"message","content":[{"type":"text","text":"carried"}]}"#;

fn scratch(tag: &str) -> std::path::PathBuf {
    peer_support::scratch_dir("egress", tag)
}

// ---------------------------------------------------------------------------
// Two Macs
// ---------------------------------------------------------------------------

/// The fixture spelling of "these are the sockets this row was paired over".
///
/// A fixture address that is not a socket address is a defect in the fixture,
/// so it panics here rather than shrinking the endpoint list quietly and
/// leaving a candidate-set assertion to fail three screens away.
fn paired_endpoints(addrs: &[String]) -> Vec<Endpoint> {
    addrs
        .iter()
        .map(|addr| {
            Endpoint::direct(
                addr.parse().expect("a fixture address is a socket address"),
                0,
                EndpointSource::Paired,
            )
        })
        .collect()
}

/// A pinned row for `peer`, with `gateway` granted or not.
fn row(peer: &PeerId, label: &str, addrs: Vec<String>, gateway: bool) -> PeerRow {
    PeerRow {
        node: *peer,
        label: label.to_string(),
        endpoints: paired_endpoints(&addrs),
        added_at: 1_767_225_600_000,
        rendezvous_secret: None,
        sees_us_at: None,
        allow: Allow {
            gateway,
            ..Allow::default()
        },
        lend: Vec::new(),
    }
}

/// The gateway's half, as `src/peer/listener.rs`'s `StreamKind::Tunnel` arm
/// runs it: accept the session, read the header, run the PRODUCTION gate, then
/// hand the stream to the PRODUCTION handler.
///
/// Test-only only in its accept loop. The three calls inside it are the three
/// the listener arm makes, in the same order, which is why this proves
/// something about the shipped path rather than about itself.
///
/// **It is no longer standing in for a missing arm.** The
/// real dispatch is landed, and
/// `the_real_listener_hands_a_tunnel_to_the_production_handler` measures it
/// end to end. What this keeps is the one thing that arm cannot give a test on
/// this box: an origin reached by ADDRESS. The arm carries with
/// `OriginRoute::Resolve`, so any test of a completed carry through it would
/// resolve `api.anthropic.com` for real.
fn gateway_on(
    listener: TcpListener,
    secret: [u8; 32],
    rows: Vec<PeerRow>,
    route: OriginRoute,
    cap_bytes: u64,
) -> tokio::task::JoinHandle<anyhow::Result<(u64, u64)>> {
    gateway_on_target(listener, secret, rows, route, cap_bytes, None)
}

/// [`gateway_on`], with the handler's target taken from the caller instead of
/// from the header.
///
/// It was built for a case that no longer exists: `TunnelTarget::Peer` could
/// not be serialized at all, so a relay header could not be written by
/// anybody and the handler's own relay refusal had to be driven directly, and
/// it is kept now for a narrower one: driving the handler with a target the
/// header did NOT carry is how a caller proves the refusal below is the
/// HANDLER's and not the gate's, which is the distinction
/// `a_relay_target_is_refused_by_a_gateway` measures.
fn gateway_on_target(
    listener: TcpListener,
    secret: [u8; 32],
    rows: Vec<PeerRow>,
    route: OriginRoute,
    cap_bytes: u64,
    target_override: Option<TunnelTarget>,
) -> tokio::task::JoinHandle<anyhow::Result<(u64, u64)>> {
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        serve_one_carry(stream, &secret, &rows, route, cap_bytes, target_override).await
    })
}

/// One accepted socket, carried the way the production listener carries it.
///
/// Split out of [`gateway_on_target`] when the exit-lock tests needed a gateway
/// that serves SEVERAL carries in a row ([`gateway_serving`]): one body, two
/// accept loops, rather than a second copy of the three production calls that
/// could drift from this one.
async fn serve_one_carry(
    mut stream: TcpStream,
    secret: &[u8; 32],
    rows: &[PeerRow],
    route: OriginRoute,
    cap_bytes: u64,
    target_override: Option<TunnelTarget>,
) -> anyhow::Result<(u64, u64)> {
    // A pinned requester on an IK return: the same authorization the
    // production responder applies, between message 1 and message 2.
    let pin_rows = rows.to_vec();
    let mut session = noise::accept_handshake(
        &mut stream,
        secret,
        noise::Handshake::Return,
        &[],
        move |remote| noise::pin_check_rows(remote, &pin_rows),
    )
    .await?;
    let frame = noise::recv_encrypted(&mut stream, &mut session.transport).await?;
    let header: StreamHeader = serde_json::from_slice(&frame)?;
    let row = rows.iter().find(|row| row.node == session.peer);
    listener::peer_stream_gate_rows(&header, row).map_err(anyhow::Error::new)?;
    // The peer the gateway charges is the one the HANDSHAKE proved, read
    // off the session before it is handed over, never anything the header
    // said about itself.
    let peer = session.peer;
    let target = target_override
        .or_else(|| header.target.clone())
        .ok_or_else(|| anyhow::anyhow!("a TUNNEL header with no target"))?;
    let budget = Mutex::new(TunnelBudget::new());
    tunnel::handle_tunnel_on(
        stream,
        session,
        Carry {
            route,
            peer,
            target: &target,
            hosts: egress::PEER_EGRESS_HOSTS,
            cap_bytes,
            budget: &budget,
            now_ms: 1_767_225_600_000,
        },
    )
    .await
}

/// A gateway that takes the carry and then writes RUBBISH into the peer
/// stream, so the requester's own pump fails to decrypt.
///
/// This is the shape the requester-side splice could not see: the splice
/// copies the PLAINTEXT half, so a frame that never decrypted reaches it as a
/// clean end of stream and is indistinguishable there from a carry that
/// finished. The failure exists only inside the pump, which is what
/// `NoiseStream::finish` reports.
fn gateway_that_corrupts(
    listener: TcpListener,
    secret: [u8; 32],
    rows: Vec<PeerRow>,
) -> tokio::task::JoinHandle<anyhow::Result<()>> {
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        let pin_rows = rows.clone();
        let mut session = noise::accept_handshake(
            &mut stream,
            &secret,
            noise::Handshake::Return,
            &[],
            move |remote| noise::pin_check_rows(remote, &pin_rows),
        )
        .await?;
        // The header the requester sends, read so the carry is genuinely
        // under way before anything goes wrong.
        let _ = noise::recv_encrypted(&mut stream, &mut session.transport).await?;
        // A frame with a valid LENGTH and a body that is not a Noise frame:
        // the framer hands it over and the decrypt is what fails, which is a
        // pump error rather than a closed socket.
        let rubbish = [0x5a_u8; 64];
        stream
            .write_all(&u32::try_from(rubbish.len())?.to_be_bytes())
            .await?;
        stream.write_all(&rubbish).await?;
        stream.flush().await?;
        // Held open: a close here would be an ordinary end of stream and the
        // pump would report nothing at all.
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        Ok(())
    })
}

/// [`gateway_on`], serving `times` carries one after another.
///
/// A pinned account is supposed to leave through the same Mac on EVERY
/// request, so the gate for it needs a Mac that is still there for the second
/// and third: a one-shot gateway would pass the same test by accident on
/// request one and prove nothing about the rest.
///
/// It returns one line per carry, the error text when there was one, so a
/// failing assertion downstream can say what the gateway saw instead of
/// leaving the reader with a bare count.
fn gateway_serving(
    listener: TcpListener,
    secret: [u8; 32],
    rows: Vec<PeerRow>,
    origin: SocketAddr,
    cap_bytes: u64,
    times: usize,
) -> tokio::task::JoinHandle<Vec<String>> {
    tokio::spawn(async move {
        let mut outcomes = Vec::new();
        for _ in 0..times {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let outcome = serve_one_carry(
                        stream,
                        &secret,
                        &rows,
                        OriginRoute::Fixed(origin),
                        cap_bytes,
                        None,
                    )
                    .await;
                    outcomes.push(match outcome {
                        Ok((up, down)) => format!("carried up={up} down={down}"),
                        Err(err) => err.to_string(),
                    });
                }
                Err(err) => outcomes.push(format!("accept failed: {err}")),
            }
        }
        outcomes
    })
}

// ---------------------------------------------------------------------------
// A fake origin that really speaks TLS
// ---------------------------------------------------------------------------

/// A CA and a leaf for [`ORIGIN`], so a real rustls client can complete a real
/// handshake through the carry.
///
/// A CA-signed leaf rather than a self-signed one: a self-signed end-entity
/// handed to a client as a trust anchor is a path webpki treats differently
/// from the production one, and the point of this fixture is that the client
/// does exactly what it does in production: validate a chain against a root it
/// trusts.
struct Origin {
    ca_pem: String,
    leaf_pem: String,
    key_pem: String,
}

fn mint_origin() -> Origin {
    use rcgen::{
        BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer,
        KeyPair, KeyUsagePurpose,
    };

    let ca_key = KeyPair::generate().expect("a CA keypair");
    let mut ca_params = CertificateParams::new(Vec::new()).expect("CA params");
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "tcr peer egress test CA");
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let ca_cert = ca_params.self_signed(&ca_key).expect("self-sign the CA");
    let ca_pem = ca_cert.pem();

    let issuer = Issuer::from_params(&ca_params, ca_key);
    let leaf_key = KeyPair::generate().expect("a leaf keypair");
    let mut leaf_params = CertificateParams::new(vec![ORIGIN.to_string()]).expect("leaf params");
    leaf_params
        .distinguished_name
        .push(DnType::CommonName, ORIGIN);
    leaf_params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    leaf_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let leaf = leaf_params
        .signed_by(&leaf_key, &issuer)
        .expect("sign the leaf");

    Origin {
        ca_pem,
        leaf_pem: leaf.pem(),
        key_pem: leaf_key.serialize_pem(),
    }
}

/// Serve exactly one TLS connection with the canned `/v1/messages` answer, and
/// report the request line the client sent through the carry.
fn fake_origin_on(
    listener: TcpListener,
    origin: &Origin,
) -> tokio::task::JoinHandle<anyhow::Result<String>> {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};

    let certs: Vec<CertificateDer<'static>> =
        rustls_pemfile::certs(&mut origin.leaf_pem.as_bytes())
            .collect::<Result<_, _>>()
            .expect("the leaf parses");
    let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut origin.key_pem.as_bytes())
        .expect("the key parses")
        .expect("the key is present");
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .expect("a server config");
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));

    tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut tls = acceptor.accept(stream).await?;
        // Read the request head and nothing past it.
        let mut head = Vec::new();
        let mut byte = [0_u8; 1];
        while head.len() < 8192 {
            let read = tls.read(&mut byte).await?;
            if read == 0 {
                break;
            }
            head.push(byte[0]);
            if head.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
             connection: close\r\n\r\n{CANNED_BODY}",
            CANNED_BODY.len()
        );
        tls.write_all(response.as_bytes()).await?;
        tls.flush().await?;
        Ok(String::from_utf8_lossy(&head).into_owned())
    })
}

/// A client that trusts the fake origin's CA and resolves [`ORIGIN`] to the
/// loopback splice. Everything else is what `src/peer/egress.rs` builds.
fn carried_client(origin: &Origin, splice_port: u16) -> reqwest::Client {
    tcr::http_client::builder()
        .no_proxy()
        // A carry that is broken rather than refused would otherwise hang this
        // suite: a mutation that dropped the first record turned a red test
        // into a four-minute wait. A request that cannot complete in ten
        // seconds against a loopback origin has failed.
        .timeout(std::time::Duration::from_secs(10))
        .add_root_certificate(
            reqwest::Certificate::from_pem(origin.ca_pem.as_bytes()).expect("the CA parses"),
        )
        .resolve(ORIGIN, SocketAddr::from(([127, 0, 0, 1], splice_port)))
        .build()
        .expect("a client")
}

/// The peers file of a Mac whose account is locked to leave from `gateway`:
/// one row for it at `addr`, the carry granted (it may carry for us), and the
/// gateway grant off (we do not carry for it).
fn write_carrying_gateway(peers: &std::path::Path, gateway: &PeerId, addr: &str) {
    let mut file = PeerFile::default();
    file.peers
        .push(row(gateway, "gateway", vec![addr.to_string()], false));
    file.peers[0].allow.carry = true;
    tcr::peer::config::save(peers, &file).expect("write the peers file");
}

/// `egress::retry_through_peer` for one `POST https://{ORIGIN}/v1/messages`
/// with an empty JSON body and no headers, against the peers file at `peers`.
async fn carry_one_message(peers: &std::path::Path) -> egress::ViaCarry {
    egress::retry_through_peer(egress::CarriedRequest {
        url: &format!("https://{ORIGIN}/v1/messages"),
        method: &axum::http::Method::POST,
        headers: axum::http::HeaderMap::new(),
        body: Some(bytes::Bytes::from_static(b"{}")),
        peers_path: peers,
    })
    .await
}

/// A TUNNEL header to `host:443`, one hop, numbered `request_id`.
fn tunnel_to_origin(host: &str, request_id: u128) -> StreamHeader {
    StreamHeader {
        kind: StreamKind::Tunnel,
        target: Some(TunnelTarget::Origin {
            host: host.to_string(),
            port: 443,
        }),
        via: Vec::new(),
        hops_remaining: 1,
        request_id,
    }
}

/// The SHIPPED peer listener on a loopback port, for a gateway whose key and
/// peers file live in `dir`, pinning `requester` with the one carry grant.
/// Returns the gateway's key and address.
///
/// The state file is inside `dir` and does not exist, so a test on it reads
/// nothing under the operator's cache directory and cannot be perturbed by an
/// operator with a pairing window open while the suite runs.
async fn real_gateway_pinning(
    dir: &std::path::Path,
    requester: &Node,
) -> (tcr::peer::id::NodeKey, SocketAddr) {
    let gateway_key = tcr::peer::id::NodeKey::load_or_mint(dir).expect("the gateway's node key");
    let peers = dir.join("tcr-peers.json");
    let mut file = PeerFile::default();
    file.peers
        .push(row(&requester.id, "requester", Vec::new(), true));
    tcr::peer::config::save(&peers, &file).expect("write the peers file");

    let listening = loopback().await;
    let gateway_addr = listening.local_addr().expect("the gateway address");
    let store = tcr::peer::config::PeerStore::open(&peers).expect("open the peers file");
    let context =
        listener::SessionContext::new(&gateway_key, store.path(), &dir.join("peer-state.json"));
    tokio::spawn(async move {
        listener::serve_on_with(listening, context)
            .await
            .log_debug("serving a test peer listener");
    });
    (gateway_key, gateway_addr)
}

/// Capture this thread's tracing events for as long as the guard lives.
///
/// `set_default`, not `set_global_default`: every `#[tokio::test]` here runs on
/// a current-thread runtime, so the tasks it spawns are polled on this same
/// thread and their events land in this collector: while another test in this
/// binary, on another thread, cannot pollute the count. A global subscriber is
/// what forced `tests/peer_refusal_log.rs` into its own binary.
fn capture() -> (Collector, tracing::subscriber::DefaultGuard) {
    use tracing_subscriber::layer::SubscriberExt as _;
    let collector = Collector::default();
    let subscriber = tracing_subscriber::registry().with(collector.clone());
    let guard = tracing::subscriber::set_default(subscriber);
    (collector, guard)
}

struct NeverRefreshes;
impl TokenRefresher for NeverRefreshes {
    fn refresh(&self, _provider: tcr::provider::Provider, _refresh_token: String) -> RefreshFuture {
        Box::pin(async { Err(OAuthError::Transient("no refresher in egress tests".into())) })
    }
}

struct NeverProbes;
impl UsageProber for NeverProbes {
    fn probe(&self, _credential: tcr::probe::ProbeCredential) -> ProbeFuture {
        Box::pin(async {
            Err(ProbeError {
                status: None,
                message: "no prober in egress tests".into(),
                retry_after_secs: None,
            })
        })
    }
}

struct NeverWarms;
impl AccountWarmer for NeverWarms {
    fn warm(&self, _access_token: String, _upstream: String) -> WarmFuture {
        Box::pin(async {
            Err(WarmError {
                status: None,
                message: "no warmer in egress tests".into(),
            })
        })
    }
}

/// One fake account. No real email, no real uuid: this repository is public.
fn fake_account() -> Account {
    Account {
        account_uuid: Some("11111111-1111-1111-1111-111111111111".to_string()),
        access_token: "at-alice".to_string(),
        refresh_token: Some("rt-alice".to_string()),
        expires_at: Some(4_102_444_800_000),
        ..Account::fixture("alice@example.com")
    }
}

/// A one-account fleet pointed at `upstream`.
fn fleet(upstream: &str) -> Arc<Manager> {
    fleet_of(upstream, &["alice@example.com"])
}

/// A fleet of one account per name, pointed at `upstream`.
///
/// Distinct names, because the carry guard is about ROTATION: a fleet of one is
/// out of moves on its first failure and can never show the difference between
/// "offered after the ladder was walked" and "offered on the first failure".
fn fleet_of(upstream: &str, names: &[&str]) -> Arc<Manager> {
    let accounts = names
        .iter()
        .enumerate()
        .map(|(n, name)| Account {
            name: (*name).to_string(),
            account_uuid: Some(format!("1111111{n}-1111-1111-1111-111111111111")),
            access_token: format!("at-{n}"),
            refresh_token: Some(format!("rt-{n}")),
            ..fake_account()
        })
        .collect();
    fleet_with(upstream, accounts)
}

/// A fleet pointed at `upstream`, carrying exactly `accounts`.
fn fleet_with(upstream: &str, accounts: Vec<Account>) -> Arc<Manager> {
    let config = Config {
        upstream: upstream.to_string(),
        switch_threshold: 0.95,
        ..Config::fixture(accounts)
    };
    Manager::new(
        config,
        Arc::new(NeverRefreshes),
        Arc::new(NeverProbes),
        Arc::new(NeverWarms),
        None,
    )
}

/// What a peer reports it carried for account 0 (`alice@example.com`): a
/// `POST /v1/messages` with no session, no model and no tool blocks, answered
/// `status` with `upstream_headers`. A test sets the fields it is about with
/// struct-update syntax.
fn carried_answer(
    status: u16,
    upstream_headers: &axum::http::HeaderMap,
) -> egress::CarriedRecord<'_> {
    egress::CarriedRecord {
        account_idx: 0,
        account: Some("alice@example.com".to_string()),
        session_key: None,
        session_kind: tcr::stats::SessionKind::Stable,
        wire_session_id: None,
        model: None,
        method: "POST".to_string(),
        path: "/v1/messages".to_string(),
        status,
        tool_uses: &[],
        tool_results: &[],
        upstream_headers,
    }
}

/// One `/v1/messages` POST through the real router.
async fn post_messages(manager: Arc<Manager>) -> axum::response::Response {
    tcr::proxy::app(manager)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"claude-opus-4-6","messages":[]}"#.to_string(),
                ))
                .expect("a request"),
        )
        .await
        .expect("the router answers")
}

/// A pinned row for `peer`, with `carry` granted or not: the field
/// [`egress::candidates_from`] actually reads, which is a different grant from
/// [`row`]'s `gateway` (that one is the gateway-side ACL, `Allow::gateway`,
/// checked by `peer_stream_gate_rows` and unrelated to which Macs THIS node
/// may ask to carry).
fn carry_row(peer: &PeerId, label: &str, addrs: Vec<String>, carry: bool) -> PeerRow {
    PeerRow {
        node: *peer,
        label: label.to_string(),
        endpoints: paired_endpoints(&addrs),
        added_at: 1_767_225_600_000,
        rendezvous_secret: None,
        sees_us_at: None,
        allow: Allow {
            carry,
            ..Allow::default()
        },
        lend: Vec::new(),
    }
}
