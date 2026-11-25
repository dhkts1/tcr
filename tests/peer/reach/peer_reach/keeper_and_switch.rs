//! `tcr peer reach` through the shipped binary, the mapping this node holds open
//! while `peer.internet` is on, and the switch reaching the keeper and the
//! carriers inside a running server.

use super::*;

// ---------------------------------------------------------------------------
// Item 4: `tcr peer reach`, through the shipped binary
// ---------------------------------------------------------------------------

/// **`tcr peer reach --json` reports the mapping a serving process holds, and
/// reports none when nothing holds one.**
///
/// The panel draws "reachable from anywhere until 18:04" off this key. It
/// could not, because the mapping lives in the memory of a thread inside a
/// running `tcr` and every other way to ask is a request to the router, which
/// REPLACES the lifetime of the mapping for that internal port (RFC 6886
/// § 3.3). A status read that shortened the thing it was reporting on is the
/// failure this record exists to avoid, and it is why the record is written by
/// the keeper and only read here.
///
/// Three cases, and the third is the one a plausible implementation gets
/// wrong. A record outlives the process that wrote it: a keeper killed with
/// `kill -9` renews nothing, and the record it left behind describes a mapping
/// the router has since dropped. So a passed `expiresAtMs` reads as no mapping
/// rather than as a mapping, and the absent case and the stale case are
/// asserted separately because one implementation passes the first and fails
/// the second.
///
/// Watched red by dropping the `.filter(|record| record.expires_at_ms >
/// now_ms)` from the read in `run_peer_reach`: the stale case then reports the
/// dead mapping. And by removing `"heldMapping"` from the payload: the live
/// case reads null.
#[test]
fn peer_reach_reports_the_mapping_a_serving_process_holds() {
    let dir = scratch("held");
    let peers = peers_file_with_one_peer(&dir, 41235);
    let state = dir.join("peer-state.json");

    let (out, err, ok) = run_reach(&peers, &["--json"]);
    assert!(ok, "tcr peer reach exited non-zero: {err}");
    let parsed: serde_json::Value =
        serde_json::from_str(out.trim()).unwrap_or_else(|why| panic!("invalid JSON: {why}\n{out}"));
    assert!(
        parsed["heldMapping"].is_null(),
        "nothing on this Mac holds a mapping, and the key says so rather than being \
         absent: {out}"
    );

    // Recorded the way a keeper records one, through the library's own writer,
    // so the file gets the 0600 the reader needs and the key the reader reads.
    let live = tcr::peer::state::MappingRecord {
        external_address: Some("203.0.113.9:41235".to_string()),
        external_port: 41235,
        internal_port: 41235,
        expires_at_ms: tcr::now_ms() + 120_000,
    };
    tcr::peer::state::save_mapping(&state, Some(live.clone())).expect("the record writes");

    let (out, err, ok) = run_reach(&peers, &["--json"]);
    assert!(ok, "tcr peer reach exited non-zero: {err}");
    let parsed: serde_json::Value =
        serde_json::from_str(out.trim()).unwrap_or_else(|why| panic!("invalid JSON: {why}\n{out}"));
    let held = &parsed["heldMapping"];
    assert_eq!(
        held["externalAddress"], "203.0.113.9:41235",
        "the socket a peer off this LAN can dial: {out}"
    );
    assert_eq!(held["externalPort"], 41235, "the port on the router: {out}");
    assert_eq!(
        held["internalPort"], 41235,
        "and the port on this Mac behind it: {out}"
    );
    assert_eq!(
        held["expiresAtMs"],
        serde_json::json!(live.expires_at_ms),
        "with the deadline the keeper recorded, unix milliseconds: {out}"
    );

    // The stale case: a keeper that died without deleting its mapping.
    tcr::peer::state::save_mapping(
        &state,
        Some(tcr::peer::state::MappingRecord {
            expires_at_ms: tcr::now_ms() - 1,
            ..live
        }),
    )
    .expect("the stale record writes");

    let (out, err, ok) = run_reach(&peers, &["--json"]);
    assert!(ok, "tcr peer reach exited non-zero: {err}");
    let parsed: serde_json::Value =
        serde_json::from_str(out.trim()).unwrap_or_else(|why| panic!("invalid JSON: {why}\n{out}"));
    assert!(
        parsed["heldMapping"].is_null(),
        "a record whose deadline has passed is not a mapping: nothing renews it and the \
         router has already dropped it: {out}"
    );
}

/// **The address a peer says it sees this Mac at survives a restart: a session
/// in one process, and `tcr peer reach` in a NEW one prints it.**
///
/// A Mac behind a NAT cannot see its own public address and the peer that just
/// greeted it can, so this is the one fact about this machine that only
/// somebody else holds, and it is what a punch aims at. It lived in a
/// process-local register, so every restart went back to "not told" until each
/// pair spoke again, which is exactly the restart the derived port already
/// learned to survive (decision row 17).
///
/// The two processes are the whole point and this test really uses two: the
/// session is stood in for by its one writer, `config::observe_seen_address`,
/// which is what the Hello arms call, and the reader is the BINARY, spawned
/// fresh with an empty register of its own. A test that called
/// `observed_self_for` in this process would pass on a register that was never
/// restored from anything.
///
/// Watched red two ways: drop the `restore_from_peers` call from
/// `run_peer_reach` (the fresh process reads its own empty register and prints
/// "not told"), and drop the write in `observe_seen_address` (there is nothing
/// on the row to restore).
#[test]
fn the_address_a_peer_sees_this_mac_at_survives_a_restart() {
    let dir = scratch("seesus");
    let peers = peers_file_with_one_peer(&dir, 41236);
    let peer = tcr_peer_wire::PeerId([0x22; 32]);
    // The public address a peer would report: documentation space, never a
    // real one, and not a socket this test opens.
    let seen: SocketAddr = "203.0.113.77:41236".parse().expect("a literal address");

    let (out, err, ok) = run_reach(&peers, &["--json"]);
    assert!(ok, "tcr peer reach exited non-zero: {err}");
    let parsed: serde_json::Value =
        serde_json::from_str(out.trim()).unwrap_or_else(|why| panic!("invalid JSON: {why}\n{out}"));
    assert!(
        parsed["peers"][0]["seesUsAt"].is_null(),
        "nothing has greeted this Mac yet, so the key is null rather than absent: {out}"
    );

    // What a completed session does, through its one writer.
    assert!(
        tcr::peer::config::observe_seen_address(&peers, &peer, seen, 1_700_000_000_000)
            .expect("the row takes the address"),
        "the first observation is a write"
    );
    assert!(
        !tcr::peer::config::observe_seen_address(&peers, &peer, seen, 1_700_000_001_000)
            .expect("the row is read again"),
        "and the same address twice is not a second write, or every session in a boot \
         rewrites this file"
    );

    let (out, err, ok) = run_reach(&peers, &["--json"]);
    assert!(ok, "tcr peer reach exited non-zero: {err}");
    let parsed: serde_json::Value =
        serde_json::from_str(out.trim()).unwrap_or_else(|why| panic!("invalid JSON: {why}\n{out}"));
    assert_eq!(
        parsed["peers"][0]["seesUsAt"], "203.0.113.77:41236",
        "a process that has run no session at all reads it off the row: {out}"
    );
    assert!(
        parsed["peers"][0]["punchPossible"].is_boolean(),
        "and says whether the two could punch, which is a yes or a no and never an \
         absence: {out}"
    );

    let (text, err, ok) = run_reach(&peers, &[]);
    assert!(ok, "tcr peer reach exited non-zero: {err}");
    assert!(
        text.contains("sees-us-at: 203.0.113.77:41236"),
        "the greppable half carries it too, in the shape the rest of this verb \
         prints: {text}"
    );
}

/// Run `tcr peer reach` against a temp peers file: `(stdout, stderr, ok)`.
fn run_reach(peers: &Path, extra: &[&str]) -> (String, String, bool) {
    let output = tcr_beside(peers)
        .args(["peer", "reach"])
        .args(["--peers", peers.to_str().expect("a utf-8 path")])
        .args(extra)
        .output()
        .expect("spawn tcr peer reach");
    (
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
        output.status.success(),
    )
}

/// **The gate for item 4**: the verb runs against a real peers file, reports
/// all three mechanisms, and asks the router for NOTHING unless told to.
///
/// The mapping assertion is the one with teeth. A NAT-PMP map request replaces
/// the lifetime of an existing mapping for the same internal port (RFC 6886
/// § 3.3), so a reachability probe that mapped by default would cut a live
/// mapping down to the probe's two minutes. `--map` is therefore opt-in, and
/// this asserts the default run says so instead of doing it.
///
/// Every router-dependent value is asserted as "present and named", never as a
/// particular address: this test has to pass on a machine with no gateway, a
/// gateway that refuses, and a gateway that answers.
#[test]
fn peer_reach_reports_the_three_mechanisms_and_maps_nothing_by_default() {
    let dir = scratch("cli");
    let peers = peers_file_with_one_peer(&dir, 41234);

    let (out, err, ok) = run_reach(&peers, &["--json"]);
    assert!(ok, "tcr peer reach exited non-zero: {err}");
    let parsed: serde_json::Value =
        serde_json::from_str(out.trim()).unwrap_or_else(|why| panic!("invalid JSON: {why}\n{out}"));

    assert!(
        parsed["ipv6"].is_array(),
        "the v6 list is always a list, empty included: {parsed}"
    );
    assert_eq!(
        parsed["listenPort"], 41234,
        "the port mapped and advertised is the configured listener's"
    );
    assert_eq!(parsed["slotSeconds"], 30);
    let slot = parsed["slot"].as_u64().expect("a slot number");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("a clock after 1970")
        .as_secs();
    let here = reach::current_slot(now);
    assert!(
        slot == here || slot + 1 == here,
        "the verb reported slot {slot} and this test computed {here}; more than one slot \
         apart means the verb is not reading the clock"
    );
    assert_eq!(
        parsed["mapping"], "not asked for (pass --map)",
        "a run without --map must write nothing to the router"
    );
    assert!(
        parsed["gateway"].is_string() && parsed["externalAddress"].is_string(),
        "both router readings are reported, refusals included: {parsed}"
    );

    let peer = &parsed["peers"][0];
    assert_eq!(peer["label"], "studio-mac");
    assert!(
        peer["node"]
            .as_str()
            .is_some_and(|id| id.starts_with("tcr-")),
        "the row names the pinned peer by its short id: {peer}"
    );
    assert!(
        peer["derivedPort"].is_null(),
        "no port may be printed while the pair's secret is not stored: {peer}"
    );
    assert!(
        peer["derivedPortUnavailable"]
            .as_str()
            .is_some_and(|why| why.contains("not stored")),
        "and the reason must be in the output, not only in a doc comment: {peer}"
    );

    // The greppable half carries the same facts, since that is the half an
    // operator reads.
    let (text, err, ok) = run_reach(&peers, &[]);
    assert!(ok, "the text run exited non-zero: {err}");
    for needle in [
        "reach: ipv6:",
        "reach: gateway:",
        "reach: external-address:",
        "reach: mapping: not asked for (pass --map)",
        "reach: listen-port: 41234",
        "reach: slot:",
        "reach: peer: studio-mac",
    ] {
        assert!(
            text.contains(needle),
            "the text output is missing {needle:?}:\n{text}"
        );
    }
}

// ---------------------------------------------------------------------------
// Item 1: the mapping this node holds open while `peer.internet` is on
// ---------------------------------------------------------------------------

/// **The gate for item 1**: the fake responder sees map, renew, delete, in
/// that order, and the keeper says it did the same three things.
///
/// The renewal interval is 50 ms here and half an hour in production
/// ([`reach::MAPPING_RENEW_INTERVAL`]); it is a parameter of
/// [`reach::run_mapping`] for exactly this reason, so the ORDER can be
/// measured without waiting out the real one.
///
/// Both halves are asserted because either alone can pass while the feature is
/// broken: the keeper's own step list is what this node believes it did, and
/// the fake's log is what the router was actually asked for.
#[test]
fn the_keeper_maps_renews_and_deletes_in_that_order() {
    let fake = FakeGateway::start(Behaviour::Cooperative {
        external_port: 41_234,
    });
    let stop = Arc::new(AtomicBool::new(false));
    let stopper = Arc::clone(&stop);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(220));
        stopper.store(true, Ordering::SeqCst);
    });

    let steps = reach::run_mapping(
        NatPmp::at(fake.addr),
        3_456,
        600,
        Duration::from_millis(50),
        &stop,
    )
    .expect("the cooperative fake maps the port");

    assert_eq!(
        steps.first(),
        Some(&reach::MappingStep::Mapped),
        "the first thing a keeper does is map: {steps:?}"
    );
    assert_eq!(
        steps.last(),
        Some(&reach::MappingStep::Deleted),
        "and the last thing it does is delete it again: {steps:?}"
    );
    assert!(
        steps
            .iter()
            .filter(|step| **step == reach::MappingStep::Renewed)
            .count()
            >= 1,
        "with at least one renewal in between, over four renewal intervals: {steps:?}"
    );
    assert_eq!(
        steps
            .iter()
            .filter(|step| **step == reach::MappingStep::Mapped)
            .count(),
        1,
        "and exactly one map: a renewal that fell back to mapping would hide a keeper \
         whose first request never ran: {steps:?}"
    );

    // The same three facts, as the router saw them. Opcode 2 is TCP; a
    // lifetime of 0 is RFC 6886's delete.
    let asked: Vec<(u8, u32)> = fake
        .asked()
        .into_iter()
        .filter(|(opcode, _)| *opcode == 2)
        .collect();
    assert!(
        asked.len() >= 3,
        "the router should have seen a map, a renewal and a delete: {asked:?}"
    );
    assert_eq!(asked[0], (2, 600), "the first TCP request is the map");
    assert_eq!(
        asked[1],
        (2, 600),
        "the second is a renewal, which is the same request again (RFC 6886 § 3.3)"
    );
    assert_eq!(
        asked.last(),
        Some(&(2, 0)),
        "and the last is the lifetime-0 delete: {asked:?}"
    );
}

/// **The gate for item 1, second half**: `tcr peer internet off` writes the
/// flag and takes the mapping away.
///
/// The gateway is handed in rather than found, so this asserts against a fake
/// on loopback and never touches the machine's real router.
#[test]
fn internet_off_writes_the_flag_and_leaves_no_mapping() {
    let fake = FakeGateway::start(Behaviour::Cooperative {
        external_port: 41_234,
    });
    let dir = scratch("internet-off");
    let peers = peers_file_with_one_peer(&dir, 41_234);

    let on = reach::set_internet_via(&peers, true, Some(NatPmp::at(fake.addr)))
        .expect("turning the switch on");
    assert_eq!(
        on,
        reach::InternetSwitch::On {
            listen_port: Some(41_234)
        },
        "turning it on names the port that will be mapped and asks the router for nothing"
    );
    assert!(
        fake.asked().is_empty(),
        "turning it ON must not write to the router: the listener asks, at boot: {:?}",
        fake.asked()
    );
    let file = std::fs::read_to_string(&peers).expect("read the peers file back");
    assert!(
        file.contains("\"internet\": true"),
        "the flag is in the file an operator can read: {file}"
    );

    let off = reach::set_internet_via(&peers, false, Some(NatPmp::at(fake.addr)))
        .expect("turning the switch off");
    let reach::InternetSwitch::Off { deleted } = off else {
        panic!("off must report a delete, not {off:?}");
    };
    let deleted = deleted.expect("the cooperative fake confirms the delete");
    assert_eq!(
        deleted.external_port, 0,
        "RFC 6886 § 3.3 requires a delete to be confirmed with external port 0"
    );
    assert_eq!(deleted.lifetime_secs, 0, "and with lifetime 0");
    assert_eq!(
        fake.asked()
            .into_iter()
            .filter(|(opcode, _)| *opcode == 2)
            .collect::<Vec<_>>(),
        vec![(2, 0)],
        "the ONLY thing the router was asked for across both switches is the delete"
    );

    let file = std::fs::read_to_string(&peers).expect("read the peers file back");
    assert!(
        file.contains("\"internet\": false"),
        "and the flag is off again: {file}"
    );
}

// ---------------------------------------------------------------------------
// `tcr peer internet off` reaches the keeper inside a running server
// ---------------------------------------------------------------------------

/// **A keeper whose `peer.internet` flag goes off deletes its mapping and
/// stops**, without anybody setting its stop flag.
///
/// The two processes are the whole of the problem. `tcr peer internet off`
/// runs in the CLI, writes the flag and deletes over NAT-PMP; the keeper lives
/// in the serving process, read `peer.internet` once at boot and kept renewing.
/// So the renewal re-created the mapping the CLI had just deleted, up to half
/// an hour later, and the operator's Mac went on being reachable from the
/// internet until it was restarted. A UPnP-granted mapping was worse: the
/// CLI's NAT-PMP delete never took that one away at all, and ending the loop
/// is what routes the delete through `MappingKeeper::delete`, which asks the
/// protocol that granted it.
///
/// Watch it fail: make the `keep_going()` check in `run_mapping_while` a
/// `false` and this test runs until its watchdog fires, on a stop flag it is
/// asserting nothing ever had to set.
#[test]
fn a_keeper_whose_internet_flag_goes_off_deletes_its_mapping_and_stops() {
    let fake = FakeGateway::start(Behaviour::Cooperative {
        external_port: 41_234,
    });
    let dir = tempfile::tempdir().expect("a temp profile directory");
    let peers_path = dir.path().join("tcr-peers.json");
    let on = PeerFile {
        internet: true,
        ..PeerFile::default()
    };
    tcr::peer::config::save(&peers_path, &on).expect("the peers file writes");

    // The switch goes off in another thread, exactly as the CLI does it: a
    // write to the file, and nothing that can reach this loop directly.
    let flipping = peers_path.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        let off = PeerFile {
            internet: false,
            ..PeerFile::default()
        };
        tcr::peer::config::save(&flipping, &off).expect("the peers file writes");
    });

    // The watchdog, so a keeper that ignores the flag ends the test instead of
    // running forever. Five seconds is far past the flag poll.
    let stop = Arc::new(AtomicBool::new(false));
    let watchdog = Arc::clone(&stop);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(5));
        watchdog.store(true, Ordering::SeqCst);
    });

    let started = Instant::now();
    let steps = reach::run_mapping_while(
        NatPmp::at(fake.addr),
        None,
        3_456,
        600,
        Duration::from_millis(50),
        &stop,
        &|| reach::internet_is_on(&peers_path),
    )
    .expect("the cooperative fake maps the port");
    let took = started.elapsed();

    assert!(
        took < Duration::from_secs(4),
        "the keeper has to notice the switch on its own; it only stopped after {took:?}, \
         which is the watchdog and not the flag"
    );
    assert!(
        !stop.load(Ordering::SeqCst),
        "and it stopped without anybody setting its stop flag"
    );
    assert_eq!(
        steps.last(),
        Some(&reach::MappingStep::Deleted),
        "a keeper that stops takes its mapping with it: {steps:?}"
    );

    // As the router saw it: the last TCP request is RFC 6886's lifetime-0
    // delete.
    let asked: Vec<(u8, u32)> = fake
        .asked()
        .into_iter()
        .filter(|(opcode, _)| *opcode == 2)
        .collect();
    assert_eq!(
        asked.last(),
        Some(&(2, 0)),
        "and the router was asked to take it away: {asked:?}"
    );
}

/// **The mapping decision a serving process takes, on all three inputs.**
///
/// Pure, so both the `internet on` arm and the loopback arm are asserted
/// without a router: `mapping_boot` is what `start_peer_mapping` routes on,
/// and `tests/peer_boot.rs` asserts the booted server reaches it.
#[test]
fn the_mapping_decision_reads_the_switch_and_the_bound_address() {
    let routable: SocketAddr = "192.168.1.4:9600"
        .parse()
        .expect("a literal address parses");
    let loopback: SocketAddr = "127.0.0.1:9600".parse().expect("a literal address parses");

    assert_eq!(
        reach::mapping_boot(true, routable),
        reach::MappingBoot::Wanted {
            internal_port: 9_600
        },
        "the switch on and a listener something can reach is the case a mapping is for"
    );
    assert_eq!(
        reach::mapping_boot(false, routable),
        reach::MappingBoot::SwitchOff,
        "the switch off asks the router for nothing"
    );
    assert_eq!(
        reach::mapping_boot(true, loopback),
        reach::MappingBoot::LoopbackOnly,
        "and a loopback listener is a mapping that would forward a public port at a socket \
         nothing off this Mac can reach"
    );
}

// ---------------------------------------------------------------------------
// The switch, and the carriers, while the process is running
// ---------------------------------------------------------------------------

/// A keeper handle with no router behind it, for driving
/// [`reach::keep_internet_mapping`] in a suite that may not ask one.
#[derive(Debug)]
struct FakeKeeper {
    /// Cleared to stand for a keeper thread that ended on its own.
    alive: Arc<AtomicBool>,
    /// Set when this handle is dropped, which is what stops a real keeper.
    dropped: Arc<AtomicBool>,
}

impl reach::KeeperHandle for FakeKeeper {
    fn is_running(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }
}

impl Drop for FakeKeeper {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

/// **The switch decides all four steps**, with no router and no file.
///
/// The pure half of `internet on|off` reaching a running server: a mapping
/// wanted with nothing running is a start, wanted with a keeper running is
/// that keeper's own renewal, and not wanted with one running is a stop. A
/// listener bound to loopback answers the same as the switch being off,
/// because a mapping there would forward a public port at a socket nothing off
/// this Mac can reach.
#[test]
fn the_internet_switch_decides_start_renew_stop_and_idle() {
    use reach::{KeeperStep, MappingBoot, keeper_step};

    let wanted = MappingBoot::Wanted {
        internal_port: 7_755,
    };
    assert_eq!(keeper_step(wanted, false), KeeperStep::Start);
    assert_eq!(keeper_step(wanted, true), KeeperStep::Renew);
    assert_eq!(keeper_step(MappingBoot::SwitchOff, true), KeeperStep::Stop);
    assert_eq!(keeper_step(MappingBoot::SwitchOff, false), KeeperStep::Idle);
    assert_eq!(
        keeper_step(MappingBoot::LoopbackOnly, true),
        KeeperStep::Stop
    );
    assert_eq!(
        keeper_step(MappingBoot::LoopbackOnly, false),
        KeeperStep::Idle
    );
}

/// **`internet on` starts a keeper on a server that is already running.**
///
/// `internet off` reached a live server: the keeper re-reads the flag between
/// renewals and the CLI deletes the mapping itself. `on` reached nothing at
/// all, because the only callers of `start_peer_mapping` were boot ones, so
/// the mapping came back at the next restart and not before, while
/// `docs/cli.md` said a running server honours both settings without one.
///
/// Driven with a fake keeper handle: the production starter asks this
/// machine's real router, which is the one thing this suite may never do. The
/// address is in the documentation range and no socket is opened on it; what
/// it decides here is only that this listener is not on loopback.
#[tokio::test]
async fn the_internet_switch_starts_a_keeper_again_on_a_running_server() {
    use tcr::peer::config::{PeerFile, save};

    let dir = tempfile::tempdir().expect("a temp profile directory");
    let peers_path = dir.path().join("tcr-peers.json");
    let local: SocketAddr = "192.0.2.7:7755".parse().expect("a literal address");
    let wake = Duration::from_millis(10);

    let write_switch = |on: bool| {
        save(
            &peers_path,
            &PeerFile {
                internet: on,
                ..PeerFile::default()
            },
        )
        .expect("the peers file writes");
    };
    let starts = Arc::new(AtomicU32::new(0));
    let new_keeper = |dropped: &Arc<AtomicBool>| FakeKeeper {
        alive: Arc::new(AtomicBool::new(true)),
        dropped: Arc::clone(dropped),
    };

    // ---- on, with a keeper running: that keeper renews itself and nothing
    // else happens.
    write_switch(true);
    let running = Arc::new(AtomicBool::new(false));
    let counted = Arc::clone(&starts);
    let held = new_keeper(&running);
    let renewing = reach::keep_internet_mapping(
        peers_path.clone(),
        local,
        wake,
        Some(2),
        move || {
            counted.fetch_add(1, Ordering::SeqCst);
            Some(FakeKeeper {
                alive: Arc::new(AtomicBool::new(true)),
                dropped: Arc::new(AtomicBool::new(false)),
            })
        },
        Some(held),
    )
    .await;
    assert_eq!(
        (renewing.started, renewing.stopped),
        (0, 0),
        "a keeper that is already running is left to renew itself"
    );
    assert_eq!(
        starts.load(Ordering::SeqCst),
        0,
        "and no second keeper is started beside it"
    );

    // ---- off, with a keeper running: the handle is dropped, which is what
    // stops a keeper and deletes the mapping.
    write_switch(false);
    let stopped_handle = Arc::new(AtomicBool::new(false));
    let counted = Arc::clone(&starts);
    let held = new_keeper(&stopped_handle);
    let stopping = reach::keep_internet_mapping(
        peers_path.clone(),
        local,
        wake,
        Some(2),
        move || {
            counted.fetch_add(1, Ordering::SeqCst);
            Some(FakeKeeper {
                alive: Arc::new(AtomicBool::new(true)),
                dropped: Arc::new(AtomicBool::new(false)),
            })
        },
        Some(held),
    )
    .await;
    assert_eq!(
        (stopping.started, stopping.stopped),
        (0, 1),
        "the switch going off has to stop the keeper this node is holding"
    );
    assert!(
        stopped_handle.load(Ordering::SeqCst),
        "and stopping it means dropping the handle, which is what deletes the mapping"
    );

    // ---- on again, with nothing running: a keeper is started, which is the
    // half that reached nothing at all.
    write_switch(true);
    let counted = Arc::clone(&starts);
    let restarted = Arc::new(AtomicBool::new(false));
    let restarting = reach::keep_internet_mapping(
        peers_path.clone(),
        local,
        wake,
        Some(2),
        move || {
            counted.fetch_add(1, Ordering::SeqCst);
            Some(FakeKeeper {
                alive: Arc::new(AtomicBool::new(true)),
                dropped: Arc::clone(&restarted),
            })
        },
        None,
    )
    .await;
    assert_eq!(
        (restarting.started, restarting.stopped),
        (1, 0),
        "`tcr peer internet on` has to reach a server that is already running, or the mapping \
         comes back at the next restart and not before"
    );
    assert_eq!(
        starts.load(Ordering::SeqCst),
        1,
        "exactly one keeper, not one per wake"
    );
}

/// **A router that would not map is not asked again at every single wake.**
///
/// A keeper whose router refuses ends within a couple of seconds of starting,
/// so "a mapping is wanted and none is running" was true at every wake of this
/// loop and the router was asked every five seconds for the life of the
/// process. The owner's log carried 169 of the resulting warning pairs in one
/// day, from one router that has no NAT-PMP service on it.
///
/// Driven with a keeper handle that is dead the moment it is handed back,
/// which is what a refused keeper looks like from here, and with a wake far
/// shorter than the first rung of the ladder so the deferral is the only
/// reason a second request would not go out.
///
/// Watch it fail: delete the `if !retry.due()` arm from `keep_internet_mapping`
/// and this reads six starts and no deferrals.
#[test]
fn a_router_that_refuses_is_asked_once_and_then_left_alone() {
    use std::sync::atomic::AtomicUsize;
    use tcr::peer::config::{PeerFile, save};
    use tracing_subscriber::layer::SubscriberExt as _;

    let dir = tempfile::tempdir().expect("a temp profile directory");
    let peers_path = dir.path().join("tcr-peers.json");
    save(
        &peers_path,
        &PeerFile {
            internet: true,
            ..PeerFile::default()
        },
    )
    .expect("the peers file writes");
    let local: SocketAddr = "192.0.2.7:7755".parse().expect("a literal address");

    let starts = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&starts);
    let capture = CaptureLayer::default();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("a current-thread runtime");

    let watched = tracing::subscriber::with_default(
        tracing_subscriber::registry().with(capture.clone()),
        || {
            runtime.block_on(reach::keep_internet_mapping(
                peers_path.clone(),
                local,
                Duration::from_millis(10),
                Some(6),
                move || {
                    counted.fetch_add(1, Ordering::SeqCst);
                    // Dead on arrival: a keeper whose router refused the first
                    // request ends on its own, and this loop sees only that.
                    Some(FakeKeeper {
                        alive: Arc::new(AtomicBool::new(false)),
                        dropped: Arc::new(AtomicBool::new(false)),
                    })
                },
                None,
            ))
        },
    );

    assert_eq!(
        starts.load(Ordering::SeqCst),
        1,
        "one request to a router that has already said no, not one per wake: {watched:?}"
    );
    assert_eq!(
        (watched.started, watched.deferred),
        (1, 5),
        "and every wake after it is counted as what it is, a router left alone until the ladder \
         says otherwise: {watched:?}"
    );

    let deferrals = capture.matching("this router would not map, so the next request waits");
    assert_eq!(
        deferrals.len(),
        5,
        "each deferral says so once, so an operator reading at DEBUG can see the wait: \
         {deferrals:?}"
    );
    assert!(
        deferrals
            .iter()
            .all(|event| event.level == tracing::Level::DEBUG),
        "and says it quietly: the loud line is the change of answer, which is written once by \
         the keeper thread: {deferrals:?}"
    );
    assert!(
        capture
            .matching("asking its router for a mapping")
            .iter()
            .all(|event| event.level == tracing::Level::INFO),
        "positive control: the capture really is seeing this loop's own lines"
    );
}
