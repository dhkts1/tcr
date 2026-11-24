// ---------------------------------------------------------------------------
// Fixtures. Obviously fake, and no account, org or workspace identity anywhere:
// a lease carries none by construction and neither does a fixture of one.
// ---------------------------------------------------------------------------

use super::*;

/// A fixed clock, so nothing here depends on when it ran.
pub(super) const NOW: i64 = 1_700_000_000_000;
pub(super) const LEASE: u128 = 0x1111_1111_1111_1111_1111_1111_1111_1111;
pub(super) const REQUEST: u128 = 0x2222_2222_2222_2222_2222_2222_2222_2222;

/// A live lease on `7d` with `budget` to spend and `spent` already gone, two
/// requests at a time, expiring five minutes after [`NOW`].
/// A fleet that can hold ANY scope, for a test whose subject is not
/// enforceability.
///
/// `Manager` answers this in production and `NoFleetUtilization` fails closed
/// (every scope but `All` is unenforceable), so a ledger test about a `Group`
/// scope needs a third answer: yes, this Mac could serve inside that scope.
/// Named rather than inlined per test, because "the scope is holdable" is the
/// premise those tests share.
pub(super) struct AnyScope;

impl serve::WindowUtilization for AnyScope {
    fn read(&self, _window: Window) -> Vec<Option<f64>> {
        Vec::new()
    }

    fn scope_restriction(&self, scope: &tcr_peer_wire::LendScope) -> serve::ScopeRestriction {
        match scope {
            tcr_peer_wire::LendScope::All => serve::ScopeRestriction::Unrestricted,
            tcr_peer_wire::LendScope::Group(name) => serve::ScopeRestriction::Group(name.clone()),
            tcr_peer_wire::LendScope::Accounts(labels) => {
                serve::ScopeRestriction::Accounts(labels.clone())
            }
        }
    }
}

pub(super) const fn lease_with(lease_id: u128, budget: f64, spent: f64) -> Lease {
    Lease {
        lease_id,
        window: Window::SevenDay,
        unit: LeaseUnit::Fraction(budget),
        granted_at_ms: NOW,
        expires_at_ms: NOW + 300_000,
        spent,
        max_inflight: 2,
        until: None,
    }
}

/// A live lease measured in TOKENS, on a clock the ledger reads as now.
///
/// Every other fixture here grants a `Fraction`, and for most gates that is the
/// right unit. It is the wrong one for anything about a token CHARGE:
/// `Ledger::tokens_of` answers `None` for a fraction by design, because the
/// tokens a share of a window buys is a fact about upstream's pricing that
/// nothing on this Mac holds. So a fraction lease charges nothing whatever the
/// meter is told, and a gate written on one passes with its own subject
/// deleted.
///
/// `now_ms` is a parameter rather than [`NOW`] for the same reason: a served
/// lease is debited against the ledger's own clock, and the fixed fixture clock
/// is two years in the past, so a lease built on it is expired before the test
/// starts.
pub(super) const fn token_lease_at(lease_id: u128, tokens: u64, now_ms: i64) -> Lease {
    Lease {
        lease_id,
        window: Window::SevenDay,
        unit: LeaseUnit::Tokens(tokens),
        granted_at_ms: now_ms,
        expires_at_ms: now_ms + 300_000,
        spent: 0.0,
        max_inflight: 2,
        until: None,
    }
}

pub(super) fn ask_for(path: &str) -> Ask<'_> {
    Ask {
        path,
        query: None,
        method: "POST",
        model: Some("claude-sonnet-4-5"),
        group: None,
        affinity: None,
        tried_local: 3,
        body: Bytes::from_static(b"{}"),
        headers: Ask::scrubbed(&client_headers()),
    }
}

/// The headers a real client sends with a message request, credentials
/// included.
///
/// `Ask::scrubbed` removes the credentials, which is the point: a borrow built from
/// this map carries `anthropic-version` and `content-type` (without which the
/// API answers 400) and carries no bearer of the client's.
pub(super) fn client_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in [
        ("content-type", "application/json"),
        ("anthropic-version", "2023-06-01"),
        ("authorization", "Bearer not-a-real-client-token"),
    ] {
        headers.insert(
            HeaderName::from_static(name),
            HeaderValue::from_static(value),
        );
    }
    headers
}

/// One pinned row in a LENDER's file: a peer it will serve for.
pub(super) fn lender_row_for(peer: PeerId, lend: Vec<LendGrant>) -> PeerRow {
    PeerRow {
        node: peer,
        label: "borrowing-mac".to_string(),
        endpoints: Vec::new(),
        added_at: NOW,
        rendezvous_secret: None,
        sees_us_at: None,
        allow: Allow {
            relay: false,
            gateway: false,
            carry: false,
            inspect: true,
            allow_disclose: false,
            accept_move: false,
            control: ControlGrants::default(),
        },
        lend,
    }
}

/// One pinned row in a BORROWER's file: a peer it may disclose to.
pub(super) fn borrower_row_for(peer: PeerId, disclose: bool, addrs: Vec<String>) -> PeerRow {
    PeerRow {
        node: peer,
        label: "lending-mac".to_string(),
        endpoints: addrs
            .iter()
            .map(|addr| {
                Endpoint::direct(
                    addr.parse().expect("a fixture address is a socket address"),
                    0,
                    EndpointSource::Paired,
                )
            })
            .collect(),
        added_at: NOW,
        rendezvous_secret: None,
        sees_us_at: None,
        allow: Allow {
            relay: false,
            gateway: false,
            carry: false,
            inspect: false,
            allow_disclose: disclose,
            accept_move: false,
            control: ControlGrants::default(),
        },
        lend: Vec::new(),
    }
}

/// Write a peers file through the program's own writer, so the mode is the one
/// `read_or_default` insists on rather than whatever a test happened to set.
pub(super) fn write_peers(path: &Path, peers: Vec<PeerRow>) {
    let file = PeerFile {
        peers,
        ..PeerFile::default()
    };
    tcr::peer::config::save(path, &file).expect("the peers file writes");
}

/// Write a lender's peers file at `path` with one row pinning `borrower` with
/// `lend`, and open it: the store a ledger test grants against.
pub(super) fn lender_store(path: &Path, borrower: PeerId, lend: Vec<LendGrant>) -> PeerStore {
    write_peers(path, vec![lender_row_for(borrower, lend)]);
    PeerStore::open(path).expect("the peers file opens")
}

/// A lender and a borrower, each with its own temp home and node key, and the
/// lender's peers file pinning the borrower with `grant`. The borrower's file is
/// not written: it needs the lender's address, which [`point_borrower_at`] adds
/// once a listener is up.
///
/// The homes are fields because they must outlive the listener: dropping one
/// deletes the lender's node key under a live socket.
pub(super) struct LeasePair {
    pub(super) lender_home: tempfile::TempDir,
    pub(super) borrower_home: tempfile::TempDir,
    pub(super) lender_peers: PathBuf,
    pub(super) borrower_peers: PathBuf,
    pub(super) lender_id: PeerId,
    pub(super) borrower_id: PeerId,
}

pub(super) fn lease_pair(grant: LendGrant) -> LeasePair {
    let lender_home = tempfile::tempdir().expect("the lender's temp home");
    let borrower_home = tempfile::tempdir().expect("the borrower's temp home");
    let lender_peers = lender_home.path().join("tcr-peers.json");
    let borrower_peers = borrower_home.path().join("tcr-peers.json");
    let lender_id = tcr::peer::id::NodeKey::load_or_mint(lender_home.path())
        .expect("the lender's key")
        .id();
    let borrower_id = tcr::peer::id::NodeKey::load_or_mint(borrower_home.path())
        .expect("the borrower's key")
        .id();
    write_peers(
        &lender_peers,
        vec![lender_row_for(borrower_id, vec![grant])],
    );
    LeasePair {
        lender_home,
        borrower_home,
        lender_peers,
        borrower_peers,
        lender_id,
        borrower_id,
    }
}

/// Lease [`LEASE`] on `7d` for `fraction` of the window, `max_inflight` at a
/// time, granted on the real clock and live for another five minutes: the
/// lease a SERVE test spends, which the ledger debits against its own clock.
pub(super) fn live_lease(fraction: f64, max_inflight: u8) -> Lease {
    let now = tcr::now_ms();
    Lease {
        lease_id: LEASE,
        window: Window::SevenDay,
        unit: LeaseUnit::Fraction(fraction),
        granted_at_ms: now,
        expires_at_ms: now + 300_000,
        spent: 0.0,
        max_inflight,
        until: None,
    }
}

/// A lender's ledger with 0.30 of `7d` headroom, holding `granted` if given.
///
/// `record_scoped` with the GRANTEE, not `record`: the review's M1 is fixed,
/// so a lease bound to nobody is a lease `enter_relay` refuses
/// (`RelayRefusal::NotTheGrantee`) and a test's borrow would be refused before
/// it reached the subject.
pub(super) fn shared_ledger(granted: Option<(Lease, PeerId)>) -> Arc<std::sync::Mutex<Ledger>> {
    let ledger = Arc::new(std::sync::Mutex::new(Ledger::new()));
    {
        let mut held = ledger.lock().expect("ledger lock");
        if let Some((lease, grantee)) = granted {
            held.record_scoped(lease, grantee, tcr_peer_wire::LendScope::All);
        }
        held.note_owner_headroom(Window::SevenDay, 0.30);
    }
    ledger
}

/// `mesh::spawn_lender` for a lender whose own fleet has never been probed
/// (`NoFleetUtilization`, so a served request is debited `MIN_DEBIT`) and whose
/// fallback manager is dry.
pub(super) async fn spawn_unprobed_lender(
    key_dir: &Path,
    peers_path: PathBuf,
    ledger: Arc<std::sync::Mutex<Ledger>>,
    upstream: String,
) -> std::net::SocketAddr {
    mesh::spawn_lender(
        key_dir.to_path_buf(),
        peers_path,
        ledger,
        upstream,
        Arc::new(serve::NoFleetUtilization),
        fleet::dry_manager(),
    )
    .await
}

/// The SERVE rig most tests here start from: [`lease_pair`] for `grant`, the
/// [`live_lease`] that grant hands out recorded in the lender's ledger for the
/// borrower, an unprobed lender serving through `upstream`, and the borrower's
/// store pointing at it.
pub(super) struct ServeRig {
    pub(super) pair: LeasePair,
    pub(super) lease: Lease,
    pub(super) ledger: Arc<std::sync::Mutex<Ledger>>,
    pub(super) peer_addr: std::net::SocketAddr,
    pub(super) store: PeerStore,
}

pub(super) async fn serve_rig(grant: LendGrant, upstream: String) -> ServeRig {
    let lease = live_lease(grant.fraction, grant.max_inflight);
    let pair = lease_pair(grant);
    let ledger = shared_ledger(Some((lease, pair.borrower_id)));
    let peer_addr = spawn_unprobed_lender(
        pair.lender_home.path(),
        pair.lender_peers.clone(),
        Arc::clone(&ledger),
        upstream,
    )
    .await;
    let store = open_borrower_store(&pair.borrower_peers, pair.lender_id, peer_addr);
    ServeRig {
        pair,
        lease,
        ledger,
        peer_addr,
        store,
    }
}

/// One more lender for a ladder of them:a fresh temp home and node key, a peers
/// file holding `row`, no lease recorded, served by [`spawn_unprobed_lender`].
///
/// Returns the home, which must outlive the listener, and the borrower's row for
/// this lender (disclosure on, at its address).
pub(super) async fn spawn_lender_in_its_own_home(
    row: PeerRow,
    upstream: String,
) -> (tempfile::TempDir, PeerRow) {
    let home = tempfile::tempdir().expect("a lender's temp home");
    let peers = home.path().join("tcr-peers.json");
    let id = tcr::peer::id::NodeKey::load_or_mint(home.path())
        .expect("the lender's key")
        .id();
    write_peers(&peers, vec![row]);
    let addr = spawn_unprobed_lender(home.path(), peers, shared_ledger(None), upstream).await;
    (home, borrower_row_for(id, true, vec![addr.to_string()]))
}

/// Write the borrower's peers file: one row for `lender`, disclosure on, at `addr`.
pub(super) fn point_borrower_at(borrower_peers: &Path, lender: PeerId, addr: std::net::SocketAddr) {
    write_peers(
        borrower_peers,
        vec![borrower_row_for(lender, true, vec![addr.to_string()])],
    );
}

/// [`point_borrower_at`], then open the borrower's store.
pub(super) fn open_borrower_store(
    borrower_peers: &Path,
    lender: PeerId,
    addr: std::net::SocketAddr,
) -> PeerStore {
    point_borrower_at(borrower_peers, lender, addr);
    PeerStore::open(borrower_peers).expect("the borrower's peers file")
}

/// The request's own frame as a capturing lender decrypted it carries the
/// request and none of the borrower's credentials.
///
/// The two presence checks are the positive control on the instrument: this is
/// the request's own frame and a non-credential header survived it, so every
/// absence after them is about the scrub and not about an empty or truncated
/// capture.
pub(super) fn assert_frame_holds_no_credential(frame: &str) {
    assert!(
        frame.contains("/v1/messages"),
        "the captured frame is the request's own: {frame}"
    );
    assert!(
        frame.contains("content-type"),
        "a non-credential header survives, so the scrub removed credentials and not the \
         request: {frame}"
    );
    for secret in BORROWER_SECRETS {
        assert!(
            !frame.contains(secret),
            "{secret} reached the lender inside a relayed frame: {frame}"
        );
    }
    for name in CREDENTIAL_HEADER_NAMES {
        assert!(
            !frame.contains(name),
            "the header NAME {name} reached the lender: {frame}"
        );
    }
}

/// The borrower's own credential-shaped secrets, named once because two tests
/// assert their absence and a second copy is a second place for one of them to
/// go missing.
///
/// Obviously fake, because this repository is public.
pub(super) const BORROWER_SECRETS: [&str; 4] = [
    "not-a-real-token",
    "not-a-real-key",
    "not-a-real-proxy-key",
    "not-a-real-cookie",
];

/// The header names the borrower's scrub removes. Read from the same place the
/// secrets above are, for the same reason.
pub(super) const CREDENTIAL_HEADER_NAMES: [&str; 4] = [
    "authorization",
    "x-api-key",
    "proxy-authorization",
    "cookie",
];

/// A borrower's client's headers with every credential shape in them, plus one
/// header that is NOT a credential so a test can tell "the scrub worked" from
/// "the frame is empty".
pub(super) fn borrower_credentials() -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in [
        ("authorization", "Bearer not-a-real-token"),
        ("x-api-key", "not-a-real-key"),
        ("proxy-authorization", "Bearer not-a-real-proxy-key"),
        ("cookie", "sessionKey=not-a-real-cookie"),
        ("content-type", "application/json"),
        // The one the API refuses a message request without, so a borrow that
        // forwards nothing is answered 400 rather than silently served.
        ("anthropic-version", "2023-06-01"),
    ] {
        headers.insert(
            HeaderName::from_static(name),
            HeaderValue::from_str(value).expect("a header value"),
        );
    }
    headers
}

/// A pinned lender/borrower pair with the lender's peers file and ledger ready.
///
/// The temp dirs are carried in the struct because they must outlive the
/// listener: dropping one deletes the lender's node key under a live socket.
pub(super) struct HostilePair {
    pub(super) lender_key: tcr::peer::id::NodeKey,
    pub(super) borrower_key: tcr::peer::id::NodeKey,
    pub(super) key_dir: PathBuf,
    pub(super) peers_path: PathBuf,
    pub(super) ledger: Arc<std::sync::Mutex<Ledger>>,
    pub(super) _homes: LeasePair,
}

pub(super) fn hostile_pair() -> HostilePair {
    let pair = lease_pair(LendGrant::new(Window::SevenDay, 0.50, 300, 5));
    // The keys `lease_pair` minted, loaded back from the homes it minted them in.
    let lender_key =
        tcr::peer::id::NodeKey::load_or_mint(pair.lender_home.path()).expect("the lender's key");
    let borrower_key = tcr::peer::id::NodeKey::load_or_mint(pair.borrower_home.path())
        .expect("the borrower's key");
    let ledger = shared_ledger(Some((live_lease(0.50, 5), pair.borrower_id)));

    HostilePair {
        lender_key,
        borrower_key,
        key_dir: pair.lender_home.path().to_path_buf(),
        peers_path: pair.lender_peers.clone(),
        ledger,
        _homes: pair,
    }
}

impl HostilePair {
    /// The pair's lender, up on a kernel port: [`spawn_unprobed_lender`] on its
    /// key, peers file and ledger, serving through `upstream`.
    pub(super) async fn spawn_lender(&self, upstream: String) -> std::net::SocketAddr {
        spawn_unprobed_lender(
            &self.key_dir,
            self.peers_path.clone(),
            Arc::clone(&self.ledger),
            upstream,
        )
        .await
    }
}

/// A hand-built SERVE frame on [`LEASE`]: request 1, no query, no headers, a
/// two-byte body, this build's wire version and the SERVE flow. A test changes
/// the one field it is about with struct-update syntax.
pub(super) fn hostile_request(method: &str, path: &str) -> serve::ServeRequest {
    serve::ServeRequest {
        lease_id: LEASE,
        request_id: 1,
        method: method.to_string(),
        path: path.to_string(),
        query: None,
        headers: Vec::new(),
        body_bytes: 2,
        proto: tcr_peer_wire::PROTO_VERSION,
        flow: serve::SERVE_FLOW,
    }
}

/// Drive one hand-built SERVE frame at a lender and read its answer.
///
/// `Err` is the lender closing the stream, which is what every refusal in
/// `handle_serve_on` does: there is no arm that explains itself to a borrower
/// whose frame should not have existed.
pub(super) async fn hostile_serve(
    peer_addr: std::net::SocketAddr,
    lender: &tcr::peer::id::NodeKey,
    borrower: &tcr::peer::id::NodeKey,
    request: serve::ServeRequest,
) -> anyhow::Result<serve::ServeReply> {
    let mut stream = tokio::net::TcpStream::connect(peer_addr)
        .await
        .expect("the lender's listener answers");
    let mut session = tcr::peer::noise::dial_handshake(
        &mut stream,
        borrower.secret_bytes(),
        tcr::peer::noise::Handshake::Return,
        Some(&lender.id().0),
        None,
    )
    .await
    .expect("a pinned peer completes the handshake");

    let header = tcr_peer_wire::StreamHeader {
        kind: tcr_peer_wire::StreamKind::Serve,
        target: None,
        via: Vec::new(),
        hops_remaining: 1,
        request_id: request.request_id,
    };
    serve::send_control(&mut stream, &mut session, &header)
        .await
        .expect("the header is written");
    serve::send_control(&mut stream, &mut session, &request)
        .await
        .expect("the frame is written");
    // THE ACK, read exactly where a borrower reads it. A refusal here is a
    // refusal before the body, so it is reported in the same word the caller
    // already reads; a lender that closed the stream instead answers `Err`,
    // which is what every `bail!` in `handle_serve_on` does.
    let ack: serve::ServeAck = serve::recv_control(&mut stream, &mut session).await?;
    if let serve::ServeAck::Refused { refusal } = ack {
        return Ok(serve::ServeReply::Refused { refusal });
    }

    // The body, and then the empty frame that ends it: a body is a run of
    // frames now, and a lender waits for the terminator before it serves.
    tcr::peer::noise::send_encrypted(&mut stream, &mut session.transport, b"{}")
        .await
        .expect("the body is written");
    tcr::peer::noise::send_encrypted(&mut stream, &mut session.transport, b"")
        .await
        .expect("the body terminator is written");

    serve::recv_control(&mut stream, &mut session).await
}

/// Write a peers file BY HAND, at mode 0600, so a test can assert what a shape
/// this program did not produce reads as.
///
/// `config::save` cannot express any of the cases below: a pre-decision-12
/// single grant object, or a hand-planted `"ended": true`, because it
/// serializes the current struct. A fixture that went through the writer would
/// only ever prove the writer agrees with itself.
pub(super) fn write_raw_peers(path: &Path, json: &str) {
    std::fs::write(path, json).expect("the fixture writes");
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .expect("0600, which is what `read_or_default` insists on");
}

/// The positive control for a test about a hand-written peers file: the modern
/// LIST shape, two grants on one row for `node`, reads as two. A reader that
/// refused or reshaped the file under test did it for that file's own reason
/// and not because it refuses every list.
pub(super) fn assert_the_list_shape_reads(dir: &Path, node: &str) {
    let path = dir.join("list-shape.json");
    write_raw_peers(
        &path,
        &format!(
            r#"{{
  "peers": [
    {{
      "node": "{node}",
      "label": "attic-nuc",
      "addedAt": 1,
      "lend": [
        {{ "window": "7d", "fraction": 0.2, "ttlS": 300, "maxInflight": 2 }},
        {{ "window": "5h", "fraction": 0.1, "ttlS": 300, "maxInflight": 1 }}
      ]
    }}
  ]
}}"#
        ),
    );
    let file = tcr::peer::config::read_or_default(&path).expect("the list shape reads");
    assert_eq!(file.peers[0].lend.len(), 2);
}

/// The built `tcr` with its `HOME` the directory holding `file`, so every call on one fixture
/// shares the files the verbs write by default and no call reaches the operator's.
pub(super) fn tcr_beside(file: &Path) -> std::process::Command {
    tcr_env::tcr(file.parent().expect("a fixture file has a directory"))
}

/// The whole `tcr peer ls --json` document, from the binary this build made.
pub(super) fn ls_json(peers_path: &Path, config_path: &Path) -> serde_json::Value {
    let out = tcr_beside(peers_path)
        .args(["peer", "ls", "--json"])
        .args(["--peers", &peers_path.display().to_string()])
        .args(["--config", &config_path.display().to_string()])
        .output()
        .expect("`tcr peer ls --json` runs");
    assert!(
        out.status.success(),
        "`tcr peer ls --json`: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("`--json` prints one JSON object")
}
