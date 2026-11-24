//! `tcr peer via`: the three words it accepts, the Mac it resolves to and never
//! substitutes, the setting stored in the peers file, and the timeout a silent
//! gateway is given.

use super::*;

// ---------------------------------------------------------------------------
// Item 3: `tcr peer via`
// ---------------------------------------------------------------------------

/// `auto`, `off`, and a peer id, and nothing else.
#[test]
fn via_parses_the_three_words_it_accepts() {
    assert_eq!(ViaSetting::parse("auto").expect("auto"), ViaSetting::auto());
    assert_eq!(ViaSetting::parse(" off ").expect("off"), ViaSetting::off());
    let peer = PeerId([9_u8; 32]);
    assert_eq!(
        ViaSetting::parse(&peer.to_wire()).expect("a peer id"),
        ViaSetting::pinned(peer)
    );
    let refusal = ViaSetting::parse("maybe").expect_err("a word that is neither");
    assert!(format!("{refusal:#}").contains("neither `auto`, `off`, nor a peer id"));
    assert_eq!(ViaSetting::default(), ViaSetting::auto());
}

/// `off` asks nobody, a pinned Mac never falls back to another one, and `auto`
/// asks the freshest first.
///
/// The substitution case is the one that matters: a silent fallback here would
/// be a request leaving by a route the operator refused.
#[test]
fn resolve_via_never_substitutes_a_mac_the_operator_did_not_choose() {
    let fresh = PeerId([1_u8; 32]);
    let stale = PeerId([2_u8; 32]);
    let unseen = PeerId([3_u8; 32]);
    let addressless = PeerId([4_u8; 32]);
    let candidates = vec![
        GatewayCandidate {
            row: row(&stale, "stale", vec!["127.0.0.1:1".to_string()], false),
            last_seen_ms: Some(1_000),
        },
        GatewayCandidate {
            row: row(&fresh, "fresh", vec!["127.0.0.1:2".to_string()], false),
            last_seen_ms: Some(9_000),
        },
        GatewayCandidate {
            row: row(&unseen, "unseen", vec!["127.0.0.1:3".to_string()], false),
            last_seen_ms: None,
        },
        GatewayCandidate {
            row: row(&addressless, "addressless", Vec::new(), false),
            last_seen_ms: Some(9_999),
        },
    ];

    assert!(egress::resolve_via(&ViaSetting::off(), &candidates).is_empty());

    let auto = egress::resolve_via(&ViaSetting::auto(), &candidates);
    assert_eq!(
        auto.iter().map(|c| c.row.node).collect::<Vec<_>>(),
        vec![fresh, stale, unseen],
        "freshest first, never-seen last, and a Mac with no address is not a candidate"
    );

    let pinned = egress::resolve_via(&ViaSetting::pinned(stale), &candidates);
    assert_eq!(
        pinned.iter().map(|c| c.row.node).collect::<Vec<_>>(),
        vec![stale],
        "a pinned choice is one Mac or none"
    );
    assert!(
        egress::resolve_via(&ViaSetting::pinned(PeerId([7_u8; 32])), &candidates).is_empty(),
        "a pinned Mac that is not a candidate must not fall back to another one"
    );
    assert!(
        egress::resolve_via(&ViaSetting::pinned(addressless), &candidates).is_empty(),
        "a pinned Mac with no address is not reachable and is not substituted"
    );
}

/// Deny by default, on the requester's side too: a host the mesh does not carry
/// is refused before a gateway is dialled.
#[tokio::test]
async fn the_requester_refuses_a_host_the_mesh_does_not_carry() {
    let gateway = node();
    let requester = node();
    for (host, port) in [
        ("example.com", 443_u16),
        (ORIGIN, 8443),
        ("platform.claude.com", 80),
    ] {
        let refusal = egress::accept_once_splice(
            &row(
                &gateway.id,
                "gateway",
                vec!["127.0.0.1:1".to_string()],
                false,
            ),
            &requester.secret,
            host,
            port,
        )
        .await
        .expect_err("a host or port off the list must be refused");
        assert!(
            format!("{refusal:#}").contains("not an origin this mesh carries"),
            "{host}:{port} must be refused by the allow-list"
        );
    }
    assert!(egress::host_allowed(ORIGIN, 443));
    assert!(egress::host_allowed("platform.claude.com", 443));
    assert!(!egress::host_allowed("api.anthropic.com.evil.test", 443));
}

// ---------------------------------------------------------------------------
// The stored setting, and the operator's knob
// ---------------------------------------------------------------------------

/// The setting is stored in the peers file and read back out of it, as the bare
/// word when there is nothing to tune and as an object when there is.
#[test]
fn via_round_trips_through_the_peers_file() {
    let peers = scratch("via-round-trip").join("tcr-peers.json");
    let mut file = PeerFile::default();

    // A file with no `via` key at all is `auto`: the default a fresh install
    // has, and the one every existing file on disk has.
    tcr::peer::config::save(&peers, &file).expect("write the peers file");
    let read = tcr::peer::config::read_or_default(&peers).expect("read it back");
    assert_eq!(egress::via_setting(&read), ViaSetting::auto());

    file.via = ViaSetting::off();
    tcr::peer::config::save(&peers, &file).expect("write `off`");
    let read = tcr::peer::config::read_or_default(&peers).expect("read `off` back");
    assert!(
        egress::via_setting(&read).is_off(),
        "the stored choice is what the request path reads"
    );
    let raw = std::fs::read_to_string(&peers).expect("the file text");
    assert!(
        raw.contains("\"via\": \"off\""),
        "with nothing tuned the key is the bare word, readable by eye: {raw}"
    );

    let pinned = PeerId([11_u8; 32]);
    file.via = ViaSetting::pinned(pinned)
        .with_setup_timeout_ms(1_500)
        .expect("1500ms is below the built-in bound");
    tcr::peer::config::save(&peers, &file).expect("write a tuned pin");
    let raw = std::fs::read_to_string(&peers).expect("the file text");
    assert!(
        raw.contains("\"setupTimeoutMs\": 1500"),
        "a tuned setting stores the number beside the word: {raw}"
    );
    let read = tcr::peer::config::read_or_default(&peers).expect("read the pin back");
    let setting = egress::via_setting(&read);
    assert_eq!(setting, file.via);
    assert_eq!(setting.to_spec(), pinned.to_wire());
    assert_eq!(
        setting.setup_timeout(),
        std::time::Duration::from_millis(1_500),
        "the request path waits the operator's number, not the constant"
    );
    assert_eq!(
        ViaSetting::auto().setup_timeout(),
        egress::CARRY_SETUP_TIMEOUT,
        "and an untuned setting is the constant"
    );

    // The knob only lowers. A carry runs after the direct path has already
    // failed, so a longer wait is worse for the caller than the answer it has.
    let refusal = ViaSetting::auto()
        .with_setup_timeout_ms(9_000)
        .expect_err("above the built-in bound");
    assert!(format!("{refusal:#}").contains("only lowers it"));
    let refusal = ViaSetting::auto()
        .with_setup_timeout_ms(0)
        .expect_err("zero is `off` spelled as a race");
    assert!(format!("{refusal:#}").contains("use `off` to mean that"));
}

/// Only a Mac inside the carry grant is a candidate, and a Mac without one is
/// not asked even when it is the only Mac pinned.
#[test]
fn only_a_mac_inside_the_carry_grant_is_a_candidate() {
    let granted = PeerId([21_u8; 32]);
    let ungranted = PeerId([22_u8; 32]);
    let mut file = PeerFile::default();
    file.peers.push(carry_row(
        &granted,
        "granted",
        vec!["127.0.0.1:1".to_string()],
        true,
    ));
    file.peers.push(carry_row(
        &ungranted,
        "ungranted",
        vec!["127.0.0.1:2".to_string()],
        false,
    ));

    let candidates = egress::candidates_from(&file, &[(granted, 1_000), (ungranted, 9_000)]);
    assert_eq!(
        candidates.iter().map(|c| c.row.node).collect::<Vec<_>>(),
        vec![granted],
        "a pinned Mac outside the carry grant is not a candidate, however fresh it is"
    );
    assert!(
        egress::resolve_via(&ViaSetting::pinned(ungranted), &candidates).is_empty(),
        "and pinning it does not put it back"
    );
}

/// A `gateway` grant is the OTHER direction (this peer may ask US to carry)
/// and does not make a row a via candidate on its own: a row that only has
/// `gateway: true` (`carry` left at its default `false`), is never offered,
/// even under `auto`.
///
/// Watch it fail by reverting [`egress::candidates_from`]'s filter to
/// `row.allow.gateway`: this row is then a candidate and the assertion below
/// goes red.
#[test]
fn a_gateway_grant_alone_does_not_make_a_row_a_carry_candidate() {
    let peer = PeerId([23_u8; 32]);
    let mut file = PeerFile::default();
    file.peers.push(PeerRow {
        node: peer,
        label: "gateway-only".to_string(),
        endpoints: paired_endpoints(&["127.0.0.1:1".to_string()]),
        added_at: 1_767_225_600_000,
        rendezvous_secret: None,
        sees_us_at: None,
        allow: Allow {
            gateway: true,
            carry: false,
            ..Allow::default()
        },
        lend: Vec::new(),
    });

    let candidates = egress::candidates_from(&file, &[(peer, 1_000)]);
    assert!(
        candidates.is_empty(),
        "a `gateway` grant with no `carry` grant is not a via candidate: {candidates:?}"
    );
    assert!(
        egress::resolve_via(&ViaSetting::auto(), &candidates).is_empty(),
        "and `auto` finds nothing to resolve, since the candidate list was already empty"
    );
}

/// `tcr peer via` writes the choice, refuses a Mac this one never pinned, and
/// refuses a timeout above the built-in bound.
///
/// Runs the binary this build produced, with `--peers` pointed at a scratch
/// file, so it reads no real config and touches no running proxy.
#[test]
fn via_writes_the_choice_and_refuses_a_mac_this_one_never_pinned() {
    let peers = scratch("via-cli").join("tcr-peers.json");
    let pinned = PeerId([31_u8; 32]);
    let stranger = PeerId([32_u8; 32]);
    let mut file = PeerFile::default();
    file.peers.push(row(
        &pinned,
        "studio-mac",
        vec!["127.0.0.1:1".to_string()],
        true,
    ));
    tcr::peer::config::save(&peers, &file).expect("write the peers file");

    let stored = |peers: &std::path::Path| {
        egress::via_setting(
            &tcr::peer::config::read_or_default(peers).expect("read the peers file"),
        )
    };

    let (out, err, ok) = run_tcr_peer(&peers, &["via", "off"]);
    assert!(ok, "`via off` must be accepted: {out}{err}");
    assert!(stored(&peers).is_off(), "and stored: {out}");

    let (out, err, ok) = run_tcr_peer(&peers, &["via", "auto"]);
    assert!(ok, "`via auto` must be accepted: {out}{err}");
    assert_eq!(stored(&peers), ViaSetting::auto());

    let (out, err, ok) = run_tcr_peer(&peers, &["via", &pinned.to_wire()]);
    assert!(ok, "a pinned Mac must be accepted: {out}{err}");
    assert_eq!(stored(&peers), ViaSetting::pinned(pinned));

    // A grant written by an unrelated command must not take the choice with
    // it: `via` is a field on the same struct every `tcr peer` write
    // round-trips, which is the whole reason it is stored there.
    let (out, err, ok) = run_tcr_peer(&peers, &["allow", &pinned.to_wire(), "gateway", "on"]);
    assert!(ok, "the unrelated write must succeed: {out}{err}");
    assert_eq!(
        stored(&peers),
        ViaSetting::pinned(pinned),
        "an unrelated `tcr peer` write must not drop the setting"
    );

    let (out, err, ok) = run_tcr_peer(&peers, &["via", &stranger.to_wire()]);
    assert!(!ok, "a Mac this one never pinned must be refused: {out}");
    assert!(
        err.contains("is not a Mac this one has pinned"),
        "and the refusal must say why: {err}"
    );
    assert_eq!(
        stored(&peers),
        ViaSetting::pinned(pinned),
        "a refused write changes nothing"
    );

    let (out, err, ok) = run_tcr_peer(&peers, &["via", "auto", "--setup-timeout-ms", "9000"]);
    assert!(
        !ok,
        "a timeout above the built-in bound must be refused: {out}"
    );
    assert!(err.contains("only lowers it"), "{err}");

    let (out, err, ok) = run_tcr_peer(&peers, &["via", "auto", "--setup-timeout-ms", "1200"]);
    assert!(ok, "lowering it must be accepted: {out}{err}");
    assert_eq!(
        stored(&peers).setup_timeout(),
        std::time::Duration::from_millis(1_200)
    );
}

/// **`tcr peer account --exits-from` writes the exit lock a panel switch
/// asks for, by peer id and by the pinned Mac's NAME, and `--must` changes the
/// strictness of a pin it was not given again.**
///
/// The panel builds an argv and reads a `--json` sibling; there was no argv
/// for this one, so an exit lock could only be set by hand-editing the config
/// this repository tells everybody never to hand-edit.
///
/// Four assertions, and the last two are the ones that catch a plausible
/// wrong implementation. A name has to resolve to the same pin the id writes,
/// or an operator reading `tcr peer ls` and typing what they see gets an
/// error or, worse, a different Mac. And `--must` on its own must leave the
/// pin alone: re-sending a pin the operator did not name is this verb
/// inventing one, and `local` is the value it would invent.
///
/// The config is a temp file. The live one at the operator's own config path
/// holds working credentials for real accounts and is never what a test
/// touches; `--config` is why this verb takes the flag at all.
///
/// Watched red three ways: return `Ok` from `set_account_egress` without
/// writing (both pins read `local`), drop the name lookup arm (the name is
/// refused as not a peer id), and apply `Some(Egress::Local)` when
/// `--exits-from` is absent (the `--must` step clears the pin).
#[test]
fn peer_account_writes_the_exit_lock_by_id_by_name_and_leaves_a_pin_alone() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let peers = dir.path().join("tcr-peers.json");
    let config = dir.path().join("tcr.json");
    let peer = node().id;
    let file = PeerFile {
        peers: vec![row(&peer, "attic-nuc", Vec::new(), true)],
        ..Default::default()
    };
    // Through the library's own writer, which is what gives the file the 0600
    // the binary refuses to read a peers file without.
    tcr::peer::config::save(&peers, &file).expect("the peers fixture writes");
    // An obviously fake account: no real email is needed for a query that
    // matches on the name, and this file is written by the binary under test.
    std::fs::write(
        &config,
        r#"{"proxy":{"port":9},"accounts":[{"name":"work-fake","accessToken":"not-a-real-token"}]}"#,
    )
    .expect("the config fixture writes");

    let wire = peer.to_wire();
    let read_pin = |label: &str| -> tcr::config::EgressPin {
        let loaded = tcr::config::load(&config)
            .unwrap_or_else(|err| panic!("the config reads back after {label}: {err}"));
        loaded.accounts[0].egress_pin()
    };

    let (out, err, ok) = run_tcr_peer(
        &peers,
        &[
            "account",
            "work-fake",
            "--config",
            config.to_str().expect("a utf-8 path"),
            "--exits-from",
            &wire,
        ],
    );
    assert!(ok, "the pin by id: {out}{err}");
    assert_eq!(
        read_pin("the pin by id").egress,
        tcr::config::Egress::Via(peer),
        "the account leaves through the Mac the id names: {out}"
    );
    assert!(
        !read_pin("the pin by id").strict,
        "and a pin nobody called must falls back rather than refusing: {out}"
    );

    let (out, err, ok) = run_tcr_peer(
        &peers,
        &[
            "account",
            "work-fake",
            "--config",
            config.to_str().expect("a utf-8 path"),
            "--exits-from",
            "local",
        ],
    );
    assert!(ok, "back to local: {out}{err}");
    assert_eq!(
        read_pin("back to local").egress,
        tcr::config::Egress::Local,
        "`local` is the word for this Mac's own socket: {out}"
    );

    let (out, err, ok) = run_tcr_peer(
        &peers,
        &[
            "account",
            "work-fake",
            "--config",
            config.to_str().expect("a utf-8 path"),
            "--exits-from",
            "attic-nuc",
        ],
    );
    assert!(
        ok,
        "the NAME a pinned Mac shows under resolves to its id: {out}{err}"
    );
    assert_eq!(
        read_pin("the pin by name").egress,
        tcr::config::Egress::Via(peer),
        "and lands the same pin the id did: {out}"
    );

    let (out, err, ok) = run_tcr_peer(
        &peers,
        &[
            "account",
            "work-fake",
            "--config",
            config.to_str().expect("a utf-8 path"),
            "--must",
        ],
    );
    assert!(ok, "--must on its own: {out}{err}");
    let after = read_pin("--must on its own");
    assert_eq!(
        after.egress,
        tcr::config::Egress::Via(peer),
        "--must changes what an unreachable exit COSTS and never where the exit is: {out}"
    );
    assert!(
        after.strict,
        "and it does change that: a request this exit cannot carry is now refused: {out}"
    );

    let (out, err, ok) = run_tcr_peer(
        &peers,
        &[
            "account",
            "work-fake",
            "--config",
            config.to_str().expect("a utf-8 path"),
            "--exits-from",
            "no-such-mac",
        ],
    );
    assert!(
        !ok,
        "a name no pinned Mac answers to is refused rather than written: {out}{err}"
    );
    assert_eq!(
        read_pin("the refused name").egress,
        tcr::config::Egress::Via(peer),
        "and the refusal leaves the pin that was there: {out}{err}"
    );
}

/// `tcr peer <args>` out of this build, never the installed binary.
fn run_tcr_peer(peers: &std::path::Path, args: &[&str]) -> (String, String, bool) {
    let home = peers
        .parent()
        .expect("the peers fixture has a directory")
        .join("cli-home");
    let config_dir = home.join(".config");
    let codex_home = home.join("codex");
    std::fs::create_dir_all(&config_dir).expect("create the fixture's config directory");
    std::fs::create_dir_all(&codex_home).expect("create the fixture's Codex directory");
    let default_config = config_dir.join("tcr.json");
    if !default_config.exists() {
        std::fs::write(&default_config, r#"{"proxy":{"port":9},"accounts":[]}"#)
            .expect("write the fixture's offline default config");
    }
    let mut command = tcr_env::tcr(&home);
    command
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("CODEX_HOME", &codex_home)
        .env("TCR_CODEX_AUTH", codex_home.join("auth.json"))
        .env(
            "TCR_CLAUDE_CODE_CREDENTIALS",
            home.join("claude-credentials.json"),
        );
    let output = command
        .arg("peer")
        .args(args)
        .arg("--peers")
        .arg(peers)
        .output()
        .expect("spawn the tcr built by this build");
    (
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
        output.status.success(),
    )
}

/// **Item 6.** `via off` dials nothing at all: no gateway sees a connection,
/// and the request is not made to wait out a setup timeout.
#[tokio::test]
async fn via_off_costs_no_dial_and_no_wait() {
    let dir = scratch("via-off");
    let peers = dir.join("tcr-peers.json");
    // A real listener, so a dial would be accepted and counted. Nothing may
    // reach it.
    let gateway = loopback().await;
    let gateway_addr = gateway.local_addr().expect("the gateway address");
    let dialled = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = Arc::clone(&dialled);
    let accepting = tokio::spawn(async move {
        while let Ok((conn, _)) = gateway.accept().await {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            drop(conn);
        }
    });

    let mut file = PeerFile::default();
    file.peers.push(row(
        &PeerId([41_u8; 32]),
        "studio-mac",
        vec![gateway_addr.to_string()],
        true,
    ));
    file.via = ViaSetting::off();
    tcr::peer::config::save(&peers, &file).expect("write the peers file");

    let (collector, _guard) = capture();
    let started = std::time::Instant::now();
    let carried = carry_one_message(&peers).await;
    let elapsed = started.elapsed();
    accepting.abort();

    assert!(
        matches!(carried, egress::ViaCarry::NotTaken),
        "`off` means no carry"
    );
    assert_eq!(
        dialled.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "`off` must not dial the Mac it is pinned to, or any other"
    );
    assert!(
        elapsed < std::time::Duration::from_millis(500),
        "`off` must cost a file read and nothing else, not a setup timeout: {elapsed:?}"
    );
    assert!(
        !collector
            .matching("never routes out through a peer")
            .is_empty(),
        "and it says so once, at debug: {:?}",
        collector.messages()
    );
    // The discriminator for the FAST PATH rather than for `off` itself:
    // `resolve_via` would also answer "no candidates" for `off`, three reads
    // later. Nothing may read the runtime state file: which logs when it is
    // missing: before the setting is honoured.
    assert!(
        collector.matching("peer state:").is_empty()
            && collector.matching("no runtime state to order").is_empty(),
        "`off` must be honoured before the runtime state is even read: {:?}",
        collector.messages()
    );
}

/// **Item 6, the lever end to end.** A gateway that accepts the connection and
/// then says nothing is abandoned at the operator's own timeout, not at the
/// five-second constant.
///
/// Watched red by making `retry_through_peer` use `CARRY_SETUP_TIMEOUT` instead
/// of `setting.setup_timeout()`: the call then takes five seconds and the
/// elapsed-time assertion fails.
#[tokio::test]
async fn the_operators_timeout_is_what_a_silent_gateway_is_given() {
    let dir = scratch("via-timeout");
    let peers = dir.join("tcr-peers.json");
    // Accepts, then holds the socket open and never speaks: the handshake
    // waits for a message that never comes, which is what the setup timeout
    // exists to bound.
    let gateway = loopback().await;
    let gateway_addr = gateway.local_addr().expect("the gateway address");
    let held = tokio::spawn(async move {
        let mut open = Vec::new();
        while let Ok((conn, _)) = gateway.accept().await {
            open.push(conn);
        }
    });

    let mut file = PeerFile::default();
    file.peers.push(carry_row(
        &PeerId([42_u8; 32]),
        "silent-mac",
        vec![gateway_addr.to_string()],
        true,
    ));
    file.via = ViaSetting::auto()
        .with_setup_timeout_ms(300)
        .expect("300ms is below the built-in bound");
    tcr::peer::config::save(&peers, &file).expect("write the peers file");

    let (collector, _guard) = capture();
    let started = std::time::Instant::now();
    let carried = carry_one_message(&peers).await;
    let elapsed = started.elapsed();
    held.abort();

    assert!(
        matches!(carried, egress::ViaCarry::NotTaken),
        "a silent Mac carries nothing"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "the operator's 300ms must bound this, not the 5s constant: {elapsed:?}"
    );
    let gave_up = collector.matching("did not answer inside the setup timeout");
    assert_eq!(
        gave_up.len(),
        1,
        "one line, naming the Mac and the bound it was given: {:?}",
        collector.messages()
    );
    assert_eq!(
        gave_up[0].fields.get("timeout_ms").map(String::as_str),
        Some("300"),
        "the line must carry the bound that was actually used: {:?}",
        gave_up[0]
    );
}

/// **A lender that TAKES the carry and never answers releases the request
/// anyway.**
///
/// The setup timeout bounded the dial and the handshake only,
/// so a Mac that completed the handshake, accepted the TUNNEL and then never
/// produced a reply held the client's request open with no bound at all:
/// `send_through` waits on a splice whose far end nobody is driving. The
/// deadline now covers the whole carry: dial, handshake and the origin's reply
///: with the operator's own value.
///
/// The far end here is the production gateway handler with a FIXED origin that
/// accepts TCP and then says nothing, which is exactly the shape that hung: the
/// carry is real and taken, the requester's ClientHello goes out, and no
/// `ServerHello` ever comes back. Nothing leaves this box.
///
/// Watched red by moving `send_through(...)` back out from under the deadline.
/// It was given a 600-second one instead, purely so the mutation run would end,
/// and it used every second of it: the assertion below failed with
/// `elapsed = 600.007306334s`. That is the measured answer to "what else bounds
/// this path": nothing does. The client `send_through` builds sets no timeout
/// of its own, so before this deadline a lender that took a carry and then
/// stayed mute held the caller's request open for as long as it cared to.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lender_that_takes_the_carry_and_never_answers_still_releases_the_request() {
    let dir = scratch("carry-deadline");
    let peers = dir.join("tcr-peers.json");
    // The requester's key has to exist BEFORE the carry runs, because the
    // gateway pins it: `retry_through_peer` mints it in this same directory.
    let requester = tcr::peer::id::NodeKey::load_or_mint(&dir).expect("mint this node");
    let gateway = node();

    // An origin that accepts and then says nothing at all. The TLS handshake
    // through the carry can therefore never complete.
    let silent_origin = loopback().await;
    let origin_addr = silent_origin.local_addr().expect("the origin address");
    let origin_task = tokio::spawn(async move {
        let mut open = Vec::new();
        while let Ok((conn, _peer)) = silent_origin.accept().await {
            open.push(conn);
        }
    });

    let gateway_listener = loopback().await;
    let gateway_addr = gateway_listener.local_addr().expect("the gateway address");
    let gateway_task = gateway_on(
        gateway_listener,
        gateway.secret,
        vec![row(&requester.id(), "requester", Vec::new(), true)],
        OriginRoute::Fixed(origin_addr),
        1024 * 1024,
    );

    let mut file = PeerFile::default();
    file.peers.push(carry_row(
        &gateway.id,
        "mute-lender",
        vec![gateway_addr.to_string()],
        true,
    ));
    file.via = ViaSetting::auto()
        .with_setup_timeout_ms(700)
        .expect("700ms is below the built-in bound");
    tcr::peer::config::save(&peers, &file).expect("write the peers file");

    let (collector, _guard) = capture();
    let started = std::time::Instant::now();
    let carried = carry_one_message(&peers).await;
    let elapsed = started.elapsed();
    origin_task.abort();
    gateway_task.abort();

    // **THE ITEM-1 GATE.** This assertion used to read `carried.is_none()`,
    // which is the same word this path answers for `via off` and for a Mac with
    // no address: the caller read it as "no carry happened", slept, and sent
    // the request again on the same account. The gateway TOOK this one, and
    // what failed afterwards is a splice that may already have put it on the
    // wire.
    let egress::ViaCarry::TakenAndFailed(gap) = &carried else {
        panic!(
            "a carry the gateway took and then dropped is not the same fact as a Mac that              never took it: this request may already have been served"
        );
    };
    assert!(
        gap.contains("took this request"),
        "the gap names what happened, and it reaches the client's 502: {gap}"
    );
    let answer = egress::delivered_unknown_response(gap);
    assert_eq!(
        answer.status().as_u16(),
        502,
        "a request that may already have run is answered, never retried"
    );
    assert_eq!(
        answer
            .headers()
            .get("x-should-retry")
            .and_then(|value| value.to_str().ok()),
        Some("false"),
        "a client that retries this pays for the request twice"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(3),
        "the client's request must be released on the carry deadline, not held \
         until something else gives up: {elapsed:?}"
    );
    let gave_up = collector.matching("never answered inside the carry deadline");
    assert_eq!(
        gave_up.len(),
        1,
        "one line, naming the Mac that took the carry: {:?}",
        collector.messages()
    );
    assert_eq!(
        gave_up[0].fields.get("timeout_ms").map(String::as_str),
        Some("700"),
        "the line must carry the bound that was actually used: {gave_up:?}"
    );
}
