//! The LAN peer mesh's side of boot: the peer listener and the loops it spawns.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::supervise;
use crate::manager::Manager;

/// Boot the proxy and return once the listener is bound.
///
/// Returns [`ServeOutcome::StoodDown`] rather than exiting when a recognized
/// proxy incumbent holds the port. Errors only when the bind itself fails.
/// The LAN peer mesh's policy file for this process, or `None` for a process
/// that must not read one.
///
/// # Why this is derived and not its own `ServeOptions` field
///
/// Every sibling side effect in [`ServeOptions`], the pin cache, the
/// Sessions/Tools cache, the usage ledger, the port claim, is a field that
/// defaults to `None` so the *dangerous* configuration is the one a caller has
/// to spell out. A `peers_path` field would read exactly the same way and is
/// the shape to reach for when the option struct is next touched; it is not
/// taken here because [`ServeOptions`] is built by struct literal in
/// `src/main.rs` and in `tests/serve_library_path.rs` with no
/// `..Default::default()`, so adding one field is a compile error in two other
/// files.
///
/// What is derived instead is not a guess. `persist_path` is the config file
/// this process may WRITE, the binary passes it, [`ServeOptions::new`] leaves
/// it `None`, and its own doc says a library caller serving a real config must
/// pass it. The peers file lives in that config's directory by the same
/// convention the rest of the peer surface already uses: `tcr peer id` resolves
/// the node key from the peers file's own directory
/// (`crate::peer::serve::node_key_dir`), which is why one `--peers` argument
/// points the whole peer surface at a temp dir. So "the peers file beside the
/// config this process owns" makes `--config` select a whole profile, its
/// accounts, its peers, its node key, instead of pairing one profile's
/// accounts with the operator's real trust relationships.
///
/// Both halves of the inertness follow: a caller with no config path reads no
/// peers file at all, and a caller with a temp config path reads a peers file
/// that does not exist, which [`crate::peer::config::read_or_default`] answers
/// as a node with no peers.
/// The file NAME is taken off [`crate::peer::config::default_path`] rather than
/// spelled again here: that function owns what the peers file is called, and a
/// second literal is the classic way a resolver comes to look for a file
/// nothing writes.
pub(super) fn peers_file_beside_config(persist_path: Option<&std::path::Path>) -> Option<PathBuf> {
    let dir = persist_path?.parent()?;
    let default = crate::peer::config::default_path();
    Some(dir.join(default.file_name()?))
}

/// Start the LAN peer mesh's listener, if this node's peers file asks for one.
///
/// # This is `listener::serve`'s production caller
///
/// Everything under `src/peer/` was reachable only from `tcr peer` subcommands
/// and from tests: the accept loop, the two-phase pairing, the stream gate and
/// the lender's half of a SERVE all existed and nothing in a serving process
/// ever called them. So a Mac running `tcr` answered no peer on any port, and
/// every gate that proved the mesh worked proved it about a test binary. This
/// function is the line that joins them.
///
/// # The switch is `listen`, and `find` alone is refused with its remedy
///
/// The ask is to boot "when the peers file has `find` on (or a listen
/// address)". `find` alone cannot get there and the refusal is deliberate: there
/// is no default peer port anywhere in this tree (the `9600` in
/// `src/peer/discovery.rs` lives only in that file's own tests), so booting on
/// `find` alone would mean inventing one here, a second place for the port to
/// live, and one the peers file does not name. That collides with the rule
/// this function exists to satisfy: **the peers file must hold
/// `listen` before the first connection**, because the beacon's port, a share
/// link and `tcr peer` all read the address off that file, and a port only this
/// process knew would make every one of them name a different socket. So a file
/// with `find` on and no `listen` gets a warning that names the remedy, and no
/// port.
///
/// # What is wired, and the one thing that is not
///
/// The accept loop gets a [`crate::peer::listener::LeaseServing`]: a fresh
/// ledger, THIS process's own proxy base (the whole of "own picker, own Bearer,
/// own bucket", see `crate::peer::serve::handle_serve_on`) and the manager as
/// the quota reader. It also gets the owner's headroom noted per window, on a
/// ticker, because `Ledger::may_relay` refuses on an ABSENT measurement: a
/// listener wired without it would answer `owner-guard` to every relayed
/// request and look, from the outside, exactly like a listener that was never
/// wired at all.
///
/// The headroom is noted for `LendScope::All` only. Per-scope headroom needs
/// the scope, which lives on `LendGrant` in `src/peer/config.rs`,
/// and nothing reads it here yet.
pub(super) async fn boot_peer_listener(
    peers_path: &std::path::Path,
    manager: &Arc<Manager>,
    own_proxy_base: &str,
    shutdown: &watch::Sender<bool>,
    background: &mut Vec<JoinHandle<()>>,
) -> Option<SocketAddr> {
    use crate::peer::listener::{self, SessionContext};
    use crate::peer::serve::LeaseServing;

    let file = match crate::peer::config::read_or_default(peers_path) {
        Ok(file) => file,
        // Never fatal, and never silent: a peers file that cannot be read is
        // not the same fact as a node with no peers, and local traffic is
        // unaffected either way. The same rule the fallback install above
        // follows, for the same reason.
        Err(err) => {
            tracing::warn!(
                path = %peers_path.display(),
                error = %err,
                "peer listener: the peers file could not be read, so no peer port is opened \
                 (local traffic is unaffected)"
            );
            return None;
        }
    };

    let Some(listen) = file.listen else {
        if file.discovery {
            tracing::warn!(
                path = %peers_path.display(),
                "peer listener: `find` is on but this peers file names no `listen` address, \
                 so there is no port to announce and none is opened; write a `listen` into \
                 the peers file (for example \"listen\": \"0.0.0.0:9600\") and restart"
            );
        }
        return None;
    };

    let listening = match listener::bind(listen).await {
        Ok(listening) => listening,
        // A peer port that will not bind must not stop this process serving
        // local traffic: the mesh is an addition to a working proxy, never a
        // precondition for one.
        Err(err) => {
            tracing::warn!(
                listen = %listen,
                error = %err,
                "peer listener: could not bind the peer socket, so no peer is answered \
                 (local traffic is unaffected)"
            );
            return None;
        }
    };
    let local = match listening.local_addr() {
        Ok(local) => local,
        Err(err) => {
            tracing::warn!(
                listen = %listen,
                error = %err,
                "peer listener: the bound peer socket has no address; not serving it"
            );
            return None;
        }
    };

    let node_dir = peers_path.parent().map_or_else(
        crate::peer::id::default_config_dir,
        std::path::Path::to_path_buf,
    );
    let key = match crate::peer::id::NodeKey::load_or_mint(&node_dir) {
        Ok(key) => key,
        Err(err) => {
            tracing::warn!(
                dir = %node_dir.display(),
                error = %err,
                "peer listener: this node has no keypair to answer a handshake with, so the \
                 peer socket is not served"
            );
            return None;
        }
    };

    // The rendezvous secrets this fleet already earned, back into the process
    // register before the listener answers anything. Without this a restart
    // loses every derived-port fallback until each pair speaks again, which is
    // precisely the case a peer that moved needs it for.
    match crate::peer::config::read_or_default(peers_path) {
        Ok(file) => {
            let restored = crate::peer::reach::restore_from_peers(&file);
            tracing::info!(restored, "peer listener: restored rendezvous secrets");
        }
        Err(err) => tracing::warn!(
            error = %err,
            "peer listener: the peers file did not read, so no rendezvous secret was restored"
        ),
    }

    // The state file that belongs with THIS peers file, not the one in the
    // operator's own cache directory: a `--config` pointing at a temp profile
    // must not write the real machine's pairing window, knock queue, mutes or
    // bans. See `state::peer_state_path`.
    let state_path = crate::peer::state::peer_state_path(peers_path);

    // The path table's boot install: this peers file's `paths` policy, plus
    // whatever this state file already carries from a probe before this boot.
    // Without this, every session that completes after boot probes into a
    // table with no memory of the last one's samples, and the EWMA never gets
    // past its first sample.
    match crate::peer::state::load(&state_path, crate::now_ms()) {
        Ok(state) => {
            let table = crate::peer::probe::PathTable::new(file.paths.clone(), state.paths);
            if let Err(err) = crate::peer::probe::install(table) {
                tracing::warn!(
                    error = %err,
                    "peer listener: could not install the path table; this boot starts with \
                     no measured paths",
                );
            }
        }
        Err(err) => tracing::warn!(
            path = %state_path.display(),
            error = %err,
            "peer listener: the state file did not read, so the path table starts with no \
             measurements from before this boot",
        ),
    }

    // RESTORED, not empty, the review's M2. `Ledger::new()` here was the whole
    // of why two doc-comments promised "restored=N expired=M" and no reader ever
    // saw the line: a restart voided every lease the operator had granted, and a
    // borrower mid-lease was stranded until it re-asked.
    let ledger = Arc::new(std::sync::Mutex::new(
        crate::peer::lease::Ledger::restored_from(&state_path),
    ));
    let context =
        SessionContext::new(&key, peers_path, &state_path).with_lease_serving(Some(LeaseServing {
            ledger: Arc::clone(&ledger),
            upstream: own_proxy_base.to_string(),
            utilization: Arc::<Manager>::clone(manager),
            manager: Arc::clone(manager),
        }));

    // The owner's headroom, noted BEFORE the accept loop starts, so the first
    // relayed request to arrive is decided against a measurement rather than
    // against an absence. Then on a ticker, because the fleet's utilization
    // moves and `may_relay` reads the last figure written.
    note_owner_headroom(manager, &ledger);
    {
        let manager = Arc::clone(manager);
        let mut stop = shutdown.subscribe();
        background.push(tokio::spawn(supervise("peer-headroom", async move {
            let notes = async {
                let mut ticker = tokio::time::interval(Duration::from_secs(30));
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    ticker.tick().await;
                    note_owner_headroom(&manager, &ledger);
                }
            };
            tokio::select! {
                _ = notes => {}
                _ = stop.changed() => {}
            }
        })));
    }

    tracing::info!(
        peer_listen = %local,
        peers_file = %peers_path.display(),
        state_file = %state_path.display(),
        // The SAME per-boot instance id the beacon announces and every knock
        // this node sends names: one `OnceLock` in `crate::peer::id`
        // (`boot_instance_id`), read by `SessionContext::new` here and by
        // `discovery::build_beacon_info` there. Logged so the two can be
        // compared in a log rather than taken on trust.
        instance = %context.instance_id(),
        node = %key.id().display(),
        "peer listener up (a second socket; the local /_tcr/ gate is untouched)"
    );

    // `peer.internet`, through the one function every serving process calls
    // for it. This used to run only inside `listener::serve`, which this
    // function does not call (it binds and serves itself, so the pairing-window
    // file is this profile's rather than the machine's): so the shipped proxy
    // asked its router for nothing, `reach::external_socket()` was `None` on
    // every Mac, and `spawn_reverse_carriers` below therefore parked a carrier
    // at every friend on every Mac, whether or not it needed one.
    //
    // The guard is moved into the listener task so the mapping is deleted when
    // that task is dropped at shutdown, which is what actually happens here:
    // see `reach::MappingGuard`.
    let mapping = crate::peer::reach::start_peer_mapping(peers_path, file.internet, local);

    // And the switch, re-read for as long as this process serves, the way the
    // beacon re-reads `peer.find`. `internet off` reached a running server
    // already, through the keeper's own re-read and the CLI's own delete;
    // `internet on` reached nothing, because the only callers of
    // `start_peer_mapping` were boot ones. `docs/cli.md` said a running server
    // honours both without a restart.
    spawn_internet_keeper(peers_path, local, mapping, shutdown, background);

    // The beacon, announced by the process that is actually listening.
    spawn_beacon(peers_path, local.port(), shutdown, background);

    // The browse, on the same cadence, writing what it heard into the state
    // file this process already owns.
    spawn_browse(peers_path, &state_path, local.port(), shutdown, background);

    // tcr's own tailnet node, when the mode is `node`: a second accept loop
    // on the node's address into the same `serve_one`. Everything it does is
    // in its own module; a build without the `tailnet-node` feature only
    // records `nodeUnsupported` there.
    {
        let node = listener::run_in_server(
            peers_path.to_path_buf(),
            state_path.clone(),
            local.port(),
            context.clone(),
            shutdown.subscribe(),
        );
        background.push(tokio::spawn(supervise("peer-tailnet-node", node)));
    }

    // The path traffic summary, on its own schedule, writing what the meter
    // charged into the same state file.
    spawn_path_traffic(&state_path, shutdown, background);

    // The dead drop's publisher, beside the internet keeper and started
    // after the path-meter task above. Boot-time gated the way
    // `file.dead_drop`'s own doc states: a Mac that never turned this on, or
    // turned it on with no store, starts no task and its behaviour is
    // bit-for-bit what it is today. `is_live` is the one check both this
    // boot decision and `keep_drops_published`'s own re-read make, so a Mac
    // that flips the switch off mid-boot stops publishing at the next tick
    // without needing a second gate here.
    if file.dead_drop.is_live() {
        match file.dead_drop.open_store() {
            Ok(store) => {
                spawn_dead_drop_publisher(peers_path, store, key.id(), local, shutdown, background);
            }
            Err(refusal) => tracing::warn!(
                error = %refusal,
                "dead drop: peer.drop is on but the configured store would not open, so \
                 nothing is published this boot"
            ),
        }
    }

    // The reverse carriers, for a Mac nothing can dial.
    //
    // Asked against the two facts `reach` measures: a router mapping this
    // process holds, and a global IPv6 address. A Mac with either one can be
    // dialled and asking a friend to hold a socket for it anyway would spend a
    // third machine's bytes on a path it does not need
    // (`tunnel::reverse_carry_is_wanted` is that decision, and it is a pure
    // function so both answers are testable without a router).
    //
    // Re-asked while this process serves, because the mapping keeper above was
    // started moments ago and its first router round trip has not landed yet.
    //
    // One keeper task per pinned friend that may carry for this node. They are
    // supervised and stopped with everything else here; each one opens a
    // carrier, waits until a forward spends it, and opens the next.
    spawn_reverse_carriers(peers_path, &key, &context, shutdown, background);

    let mut stop = shutdown.subscribe();
    background.push(tokio::spawn(supervise("peer-listener", async move {
        tokio::select! {
            result = listener::serve_on_with(listening, context) => {
                if let Err(err) = result {
                    tracing::warn!(error = %err, "peer listener: the accept loop stopped");
                }
            }
            _ = stop.changed() => {}
        }
    })));

    Some(local)
}

/// Announce this node on the LAN, from the process that is actually
/// listening, for as long as `peer.find` is on.
///
/// # Why the announcer belongs here and not in the CLI
///
/// `tcr peer find on` used to register the beacon in the CLI process and
/// exit. mdns-sd runs its daemon on a background thread of that process, so
/// the beacon died with the command: a `find off` in a new process found the
/// shared daemon empty, and no serving process ever announced anything. The
/// payload made it worse than a missing row. A beacon carries the announcing
/// process's per-boot instance id
/// ([`crate::peer::id::boot_instance_id`]), and the knock a neighbour sends
/// after seeing it names the id it saw: announced from the CLI, that id
/// belonged to a process that had already exited, so it could never match the
/// server's own.
///
/// # What the loop does
///
/// It wakes every [`crate::peer::discovery::BEACON_RESTAMP_INTERVAL`] and asks
/// [`crate::peer::discovery::announce_step`] what to do, off two facts: the
/// `find` flag as the peers file states it NOW, and the minute the beacon it
/// is holding was stamped in.
///
/// The flag is re-read from the file rather than taken from the boot snapshot,
/// which is how `tcr peer find on|off` reaches a running server at all: the
/// same shape as `Manager::reload_groups_if_changed`, reading the file each
/// wake because this one is small and twenty seconds apart.
///
/// The re-stamp is the second half. The beacon's keyed tag is computed over
/// the current minute and a receiver holding the network key accepts this
/// minute and the one before, so a beacon stamped once at registration stops
/// verifying about two minutes later: the node kept announcing and became
/// undiscoverable to exactly the neighbours that share its key.
fn spawn_beacon(
    peers_path: &std::path::Path,
    port: u16,
    shutdown: &watch::Sender<bool>,
    background: &mut Vec<JoinHandle<()>>,
) {
    use crate::peer::discovery::{self, AnnounceStep};

    let peers_path = peers_path.to_path_buf();
    let mut stop = shutdown.subscribe();
    background.push(tokio::spawn(supervise("peer-beacon", async move {
        let announcing = async move {
            let mut stamped: Option<i64> = None;
            let mut ticker = tokio::time::interval(discovery::BEACON_RESTAMP_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                let file = match crate::peer::config::read_or_default(&peers_path) {
                    Ok(file) => file,
                    // Never fatal: a peers file that does not read for a
                    // moment is not the same fact as `find` being off, and
                    // unregistering on it would take a working beacon away.
                    Err(err) => {
                        tracing::warn!(
                            path = %peers_path.display(),
                            error = %err,
                            "peer find: the peers file did not read, so the beacon is left as \
                             it is until the next wake"
                        );
                        continue;
                    }
                };
                match discovery::announce_step(
                    file.discovery,
                    stamped,
                    crate::now_ms().div_euclid(1_000),
                ) {
                    AnnounceStep::Idle => {}
                    step @ (AnnounceStep::Start | AnnounceStep::Restamp) => {
                        let store = match crate::peer::config::PeerStore::open(&peers_path) {
                            Ok(store) => store,
                            Err(err) => {
                                tracing::warn!(
                                    error = %err,
                                    "peer find: the peers file did not open, so nothing is \
                                     announced this wake"
                                );
                                continue;
                            }
                        };
                        let name = file.announce_name.then(|| file.display_name());
                        match discovery::advertise(&store, name.as_deref(), port).await {
                            Ok(()) => {
                                stamped = Some(discovery::current_stamp_minute());
                                if step == AnnounceStep::Start {
                                    tracing::info!(
                                        peer_port = port,
                                        named = file.announce_name,
                                        "peer find: announcing this node on the LAN"
                                    );
                                }
                            }
                            Err(err) => tracing::warn!(
                                error = %err,
                                "peer find: this node could not announce itself on the LAN; \
                                 it is still reachable by address"
                            ),
                        }
                    }
                    AnnounceStep::Stop => {
                        stamped = None;
                        if let Err(err) = discovery::stop_all() {
                            tracing::warn!(
                                error = %err,
                                "peer find: off, and the beacon could not be withdrawn; it \
                                 ages out on its own"
                            );
                        } else {
                            tracing::info!("peer find: off, so this node stopped announcing");
                        }
                    }
                }
            }
        };
        tokio::select! {
            _ = announcing => {}
            _ = stop.changed() => {
                if let Err(err) = discovery::stop_all() {
                    tracing::warn!(
                        error = %err,
                        "peer find: the beacon could not be withdrawn at shutdown; it ages \
                         out on its own"
                    );
                }
            }
        }
    })));
}

/// Browse for other nodes' beacons, from the process that is actually
/// listening, for as long as `peer.find` is on, and write what it heard into
/// this profile's state file.
///
/// A twin of [`spawn_beacon`]: `supervise`, a ticker on
/// [`crate::peer::discovery::BROWSE_INTERVAL`] with `MissedTickBehavior::Skip`,
/// and the `find` flag re-read off the peers file each wake rather than taken
/// from the boot snapshot, the same reason [`spawn_beacon`] gives.
///
/// Holds a [`discovery::FoundList`] across wakes: the caps and the ageing
/// live there, in memory, and this task's whole job after each browse is
/// dropping this node's own beacon (never by address, only by instance id;
/// see the module's own refusal) and writing [`discovery::FoundList::shown`]
/// and [`discovery::FoundList::not_shown`] into `peer-state.json`.
///
/// A peers file, or a state file, that does not read or write for a moment
/// is not the same fact as `find` being off: logged and left as it is until
/// the next wake, the same containment [`spawn_beacon`] already has.
///
/// # The second source: the tailnet, in app mode
///
/// The same task is the only writer of the found list, because
/// `state::save_found` replaces the whole section and a second writer would
/// erase the first one's rows every wake. So it also owns the tailnet beacon
/// socket ([`crate::peer::tailnet_find`]): between wakes it reads datagrams
/// from the operator's own Macs, and on each wake it reads the Tailscale app
/// (on a blocking thread, since that runs a process), binds or rebinds the
/// socket to this Mac's own tailnet IPv4, sends this Mac's beacon to each own
/// Mac, merges what it heard with the mDNS scan, writes the found list once,
/// and writes `held` (`state::save_tailnet`) while the mode is app.
fn spawn_browse(
    peers_path: &std::path::Path,
    state_path: &std::path::Path,
    port: u16,
    shutdown: &watch::Sender<bool>,
    background: &mut Vec<JoinHandle<()>>,
) {
    use crate::peer::config::TailnetMode;
    use crate::peer::discovery;
    use crate::peer::state::FoundSource;
    use crate::peer::tailnet_find::{self, BeaconIntake, BeaconSocket};

    let peers_path = peers_path.to_path_buf();
    let state_path = state_path.to_path_buf();
    let mut stop = shutdown.subscribe();
    background.push(tokio::spawn(supervise("peer-browse", async move {
        let browsing = async move {
            let mut found = discovery::FoundList::new();
            let mut ticker = tokio::time::interval(discovery::BROWSE_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut socket = BeaconSocket::new(port);
            let mut intake = BeaconIntake::new();
            // The latest reading's own-Mac addresses, and the network key as
            // of the last wake: what a datagram between wakes is checked
            // against.
            let mut own_macs: Vec<std::net::Ipv4Addr> = Vec::new();
            let mut network_key: Option<crate::peer::config::NetworkKey> = None;
            // The address a bind last failed on, so the warning is logged
            // once per address rather than every wake.
            let mut bind_failed_on: Option<std::net::Ipv4Addr> = None;
            let mut buf = [0_u8; tailnet_find::RECEIVE_BUFFER_BYTES];
            loop {
                let received = tokio::select! {
                    _ = ticker.tick() => None,
                    received = socket.recv(&mut buf) => Some(received),
                };
                if let Some(received) = received {
                    match received {
                        Ok((len, from)) => {
                            let intake_result = intake.offer(
                                from.ip(),
                                &buf[..len],
                                &own_macs,
                                network_key.as_ref(),
                                crate::now_ms(),
                            );
                            tracing::debug!(
                                from = %from,
                                outcome = ?intake_result,
                                "peer find: a tailnet datagram"
                            );
                        }
                        // Closed rather than read again, so an error that
                        // repeats cannot spin this arm; the next wake binds
                        // a fresh socket.
                        Err(err) => {
                            socket.close();
                            tracing::debug!(
                                error = %err,
                                "peer find: the tailnet beacon socket failed; it is reopened \
                                 on the next wake"
                            );
                        }
                    }
                    continue;
                }
                let file = match crate::peer::config::read_or_default(&peers_path) {
                    Ok(file) => file,
                    Err(err) => {
                        tracing::warn!(
                            path = %peers_path.display(),
                            error = %err,
                            "peer find: the peers file did not read, so the found list is left \
                             as it is until the next wake"
                        );
                        continue;
                    }
                };
                network_key = file.network_key;

                // The tailnet source, read only in app mode. The read runs a
                // process under a deadline, so it goes to a blocking thread,
                // and it refreshes the reading the peer listener decides an
                // own-Mac knock on.
                let mode = file.tailnet.effective();
                let app = match mode {
                    TailnetMode::App => Some(
                        tailnet_find::OWN_MAC_READINGS
                            .refresh(tailnet_find::read_the_app)
                            .await,
                    ),
                    TailnetMode::Off | TailnetMode::Node => None,
                };
                let mut scan: Vec<discovery::Discovered> = Vec::new();
                let mut tailnet_peers = Vec::new();
                if let Some(Ok(status)) = &app {
                    {
                        own_macs = tailnet_find::own_mac_addresses(status);
                        match status.own_ipv4() {
                            Some(own) => match socket.bind_to(own).await {
                                Ok(()) => bind_failed_on = None,
                                Err(err) => {
                                    if bind_failed_on != Some(own) {
                                        tracing::warn!(
                                            address = %own,
                                            peer_port = port,
                                            error = %err,
                                            "peer find: could not listen for tailnet beacons on \
                                             this Mac's tailnet address; your other Macs are \
                                             not found over the tailnet until it binds"
                                        );
                                    }
                                    bind_failed_on = Some(own);
                                }
                            },
                            None => {
                                socket.close();
                            }
                        }
                        let name = file.announce_name.then(|| file.display_name());
                        let beacon =
                            discovery::tailnet_beacon(port, name.as_deref(), network_key.as_ref());
                        socket.send(&own_macs, &beacon).await;
                        scan.extend(intake.take(&own_macs));
                        tailnet_peers = tailnet_find::pinned_peers(status, &file.peers);
                    }
                } else {
                    // Off, node, or a read that failed: no reading to trust,
                    // so nothing is heard and nothing heard before is shown.
                    own_macs.clear();
                    socket.close();
                    intake.clear();
                    found.forget(FoundSource::Tailnet);
                }

                // The LAN source. `AnnounceStep` is the beacon's own re-stamp
                // decision and not what this task reads; the flag itself is:
                // browsing while `find` is off would show rows an operator
                // who turned discovery off never asked to see.
                if file.discovery {
                    match crate::peer::config::PeerStore::open(&peers_path) {
                        Ok(store) => match discovery::browse(&store).await {
                            Ok(lan) => scan.extend(lan),
                            Err(err) => tracing::warn!(
                                error = %err,
                                "peer find: this node could not browse the LAN this wake"
                            ),
                        },
                        Err(err) => tracing::warn!(
                            error = %err,
                            "peer find: the peers file did not open, so no browse ran this wake"
                        ),
                    }
                } else {
                    found.forget(FoundSource::Lan);
                }

                let now_ms = crate::now_ms();
                if file.discovery || mode == TailnetMode::App {
                    // Never by address: one Mac has several, and two
                    // containers share a host. The instance id is the only
                    // sound key for "this is my own beacon", over either
                    // source.
                    let this_instance = crate::peer::id::boot_instance_id();
                    scan.retain(|row| row.instance_id != this_instance);
                    found.observe(scan, now_ms);
                    let rows: Vec<crate::peer::state::Found> = found
                        .shown()
                        .iter()
                        .map(|row| row.found_row(now_ms))
                        .collect();
                    if let Err(err) =
                        crate::peer::state::save_found(&state_path, rows, found.not_shown(), now_ms)
                    {
                        tracing::warn!(
                            path = %state_path.display(),
                            error = %err,
                            "peer find: the found list did not write, so the last scan's rows \
                             are only in this process's memory"
                        );
                    }
                }

                // `held`, on every wake while the mode is app, so `tcr peer
                // tailnet --json` and the panel can tell a server acting on
                // the setting from one that stopped writing. Off and node
                // write nothing from here: see `tailnet_find::write_held`.
                let reading = app
                    .as_ref()
                    .map(|result| result.as_ref().map(|status| &**status).map_err(|err| *err));
                if let Err(err) = tailnet_find::write_held(
                    &state_path,
                    mode,
                    reading,
                    found.heard_from(FoundSource::Tailnet),
                    tailnet_peers,
                    now_ms,
                ) {
                    tracing::warn!(
                        path = %state_path.display(),
                        error = %err,
                        "peer find: the tailnet record did not write"
                    );
                }
            }
        };
        tokio::select! {
            _ = browsing => {}
            _ = stop.changed() => {}
        }
    })));
}

/// Sum this process's path meter into `peer-state.json`, on
/// [`crate::peer::tunnel::PATH_TRAFFIC_SUM_INTERVAL`], for as long as this
/// process serves.
///
/// A twin of [`spawn_browse`]: `supervise`, a ticker with
/// `MissedTickBehavior::Skip`, `tokio::select!` against `stop.changed()`. It
/// needs only `state_path`, because the meter
/// [`crate::peer::tunnel::path_meter`] reads is process-global rather than
/// threaded from boot; nothing here charges it, three other call sites do.
///
/// The byte cap itself stays in memory: see [`crate::peer::tunnel::TunnelBudget`]'s
/// own doc for why a restart resetting it is the design, not a gap this task
/// is meant to close.
///
/// Each wake calls [`path_traffic_tick`], the one place that decides whether
/// this wake's rows are worth a write.
fn spawn_path_traffic(
    state_path: &std::path::Path,
    shutdown: &watch::Sender<bool>,
    background: &mut Vec<JoinHandle<()>>,
) {
    let state_path = state_path.to_path_buf();
    let mut stop = shutdown.subscribe();
    background.push(tokio::spawn(supervise("peer-path-traffic", async move {
        let summing = async move {
            let mut ticker = tokio::time::interval(crate::peer::tunnel::PATH_TRAFFIC_SUM_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut last_written: Vec<crate::peer::state::PathTraffic> = Vec::new();
            loop {
                ticker.tick().await;
                path_traffic_tick(&state_path, &mut last_written);
            }
        };
        tokio::select! {
            _ = summing => {}
            _ = stop.changed() => {}
        }
    })));
}

/// One wake of [`spawn_path_traffic`]'s loop, and the whole of its wiring: sum
/// the process-global meter, and write it to `state_path` only when the rows
/// changed since `last_written`, updating `last_written` on a landed write.
///
/// A named function called from exactly one place rather than inlined in the
/// loop above, so a test can drive the production wire directly instead of
/// standing up a whole task and waiting on its ticker.
///
/// "Changed" ignores `updated_at_ms`: [`path_traffic_rows_match`] compares
/// peer, locator, `bytes_last_hour` and `tokens_last_hour`, because
/// `updated_at_ms` is the instant this call summed at and differs on every
/// wake by construction. A path whose traffic rolled off since the last write
/// still counts as changed: [`crate::peer::tunnel::PathMeter::rows`] trims the
/// window first, so a stale row comes back with zeroes, and zero is a
/// different fact from the figure last written.
pub(super) fn path_traffic_tick(
    state_path: &std::path::Path,
    last_written: &mut Vec<crate::peer::state::PathTraffic>,
) {
    let now_ms = crate::now_ms();
    let rows = if let Ok(mut meter) = crate::peer::tunnel::path_meter().lock() {
        meter.rows(now_ms)
    } else {
        tracing::warn!(
            "peer path traffic: the path meter's lock is poisoned, so this wake summed \
             nothing"
        );
        return;
    };
    if path_traffic_rows_match(&rows, last_written) {
        return;
    }
    match crate::peer::state::save_path_traffic(state_path, &rows) {
        Ok(()) => *last_written = rows,
        Err(err) => tracing::warn!(
            path = %state_path.display(),
            error = %err,
            "peer path traffic: this wake's totals did not write, so peer-state.json is behind \
             this process's meter"
        ),
    }
}

/// Whether `rows` says nothing beyond what `last_written` already says,
/// ignoring `updated_at_ms` for [`path_traffic_tick`]'s reason.
///
/// Order-independent: [`crate::peer::tunnel::PathMeter::rows`] iterates a set,
/// so two sweeps over the same paths are not guaranteed to list them in the
/// same order.
fn path_traffic_rows_match(
    rows: &[crate::peer::state::PathTraffic],
    last_written: &[crate::peer::state::PathTraffic],
) -> bool {
    if rows.len() != last_written.len() {
        return false;
    }
    let key = |row: &crate::peer::state::PathTraffic| {
        (
            row.peer.to_wire(),
            format!("{:?}", row.locator),
            row.bytes_last_hour,
            row.tokens_last_hour,
        )
    };
    let mut a: Vec<_> = rows.iter().map(key).collect();
    let mut b: Vec<_> = last_written.iter().map(key).collect();
    a.sort();
    b.sort();
    a == b
}

/// How often the dead-drop publisher wakes and, for every friend it is
/// allowed to, writes this Mac's current address.
///
/// [`crate::peer::drop::publish_for`] has no floor of its own: it writes
/// every time it is called and allowed to, so this interval is the entire
/// cadence. Five minutes, well above the store round trip and well under
/// [`crate::peer::drop::MAX_RECORD_AGE`] (two slots, two hours), so a record
/// this Mac holds is refreshed several times before the slot it published
/// into stops being accepted.
const DEAD_DROP_PUBLISH_INTERVAL: Duration = Duration::from_secs(300);

/// Keep this Mac's dead drop current, for every friend `allow.control.drop`
/// covers, until shutdown.
///
/// The loop is [`crate::peer::drop::keep_drops_published`]; what is here is
/// the wiring, [`spawn_internet_keeper`]'s shape. The boot-time gate on
/// [`crate::peer::config::DeadDropConfig::is_live`] is the caller's, not
/// this function's, so a Mac that never turned the drop on never calls this
/// at all.
fn spawn_dead_drop_publisher(
    peers_path: &std::path::Path,
    store: crate::peer::drop::HttpsTemplateStore,
    us: tcr_peer_wire::PeerId,
    local: SocketAddr,
    shutdown: &watch::Sender<bool>,
    background: &mut Vec<JoinHandle<()>>,
) {
    let peers_path = peers_path.to_path_buf();
    let mut stop = shutdown.subscribe();
    background.push(tokio::spawn(supervise("peer-dead-drop", async move {
        let publishing = crate::peer::drop::keep_drops_published(
            store,
            peers_path,
            us,
            local,
            DEAD_DROP_PUBLISH_INTERVAL,
        );
        tokio::select! {
            () = publishing => {}
            _ = stop.changed() => {}
        }
    })));
}

/// Keep this process's router mapping in step with `peer.internet`.
///
/// Holds the keeper the boot decision started, if any, and is the only thing
/// that holds it: dropping the handle stops the keeper, which deletes the
/// mapping over the protocol that granted it, and this task is dropped at
/// shutdown with everything else here.
///
/// The decision is [`crate::peer::reach::keeper_step`] and the loop is
/// [`crate::peer::reach::keep_internet_mapping`]; what is here is the wiring
/// and the one thing a test may not do, which is ask this machine's real
/// router.
fn spawn_internet_keeper(
    peers_path: &std::path::Path,
    local: SocketAddr,
    held: Option<crate::peer::reach::MappingGuard>,
    shutdown: &watch::Sender<bool>,
    background: &mut Vec<JoinHandle<()>>,
) {
    let peers_path = peers_path.to_path_buf();
    let mut stop = shutdown.subscribe();
    background.push(tokio::spawn(supervise("peer-internet", async move {
        let starting = peers_path.clone();
        let watching = crate::peer::reach::keep_internet_mapping(
            peers_path,
            local,
            crate::peer::reach::INTERNET_POLL_INTERVAL,
            None,
            move || crate::peer::reach::start_peer_mapping(&starting, true, local),
            held,
        );
        tokio::select! {
            _ = watching => {}
            _ = stop.changed() => {}
        }
    })));
}

/// Keep one carrier keeper per friend running for as long as nothing can dial
/// this Mac.
///
/// Split out of [`boot_peer_listener`] because it is a decision and a fan-out
/// rather than another line of wiring: the decision is
/// [`crate::peer::tunnel::reverse_carry_is_wanted`], and the fan-out is one
/// task per friend that `forwarders_for` says may carry for this node.
///
/// # Why the decision is re-asked, and what it costs
///
/// It was taken once, here, two statements after the mapping keeper thread was
/// spawned: before its first NAT-PMP round trip could publish anything. So
/// `reach::external_socket()` was `None` on every boot, and a Mac its router
/// would map perfectly well parked `MAX_PARKED_PER_PEER` carriers at every
/// friend it had, seconds before it became dialable. The boot test binds
/// loopback, where no mapping is ever asked for, so nothing observed the
/// ordering.
///
/// Re-asking costs no router traffic: both facts are read out of this
/// process's own memory, the mapping from the register the keeper publishes to
/// and the addresses from this machine's own interfaces. A probe on a loop
/// would be the expensive answer, and it would also replace the lifetime of
/// the mapping it was asking about (`tcr peer reach`'s own `--map` is opt-in
/// for exactly that reason).
fn spawn_reverse_carriers(
    peers_path: &std::path::Path,
    key: &crate::peer::id::NodeKey,
    context: &crate::peer::listener::SessionContext,
    shutdown: &watch::Sender<bool>,
    background: &mut Vec<JoinHandle<()>>,
) {
    let peers_path = peers_path.to_path_buf();
    let node = key.id();
    let context = context.clone();
    let mut stop = shutdown.subscribe();
    background.push(tokio::spawn(supervise("peer-reverse", async move {
        let watching = crate::peer::tunnel::watch_reverse_need(
            crate::peer::tunnel::REVERSE_RECHECK_INTERVAL,
            None,
            reverse_need_now,
            || park_reverse_carriers(&peers_path, &node, &context),
            |carriers: watch::Sender<bool>| {
                // Every keeper for this fan-out selects on this channel, so
                // one send ends all of them; a receiver that has already gone
                // is not an error to report.
                carriers.send_replace(true);
            },
            None,
        );
        tokio::select! {
            _ = watching => {}
            _ = stop.changed() => {}
        }
    })));
}

/// Whether this Mac needs a friend to hold a carrier for it, right now.
///
/// `external_socket` answers with the mapped socket rather than the `Mapping`
/// the decision takes, so the presence of one is converted here and the
/// absence of one stays an absence: a node with no mapping and no IPv6 is the
/// whole of what `Wanted` means.
fn reverse_need_now() -> crate::peer::tunnel::ReverseNeed {
    let mapping = crate::peer::reach::external_socket().map(|socket| crate::peer::reach::Mapping {
        protocol: crate::peer::reach::MapProtocol::Tcp,
        internal_port: socket.port(),
        external_port: socket.port(),
        lifetime_secs: 0,
        epoch_secs: 0,
    });
    crate::peer::tunnel::reverse_carry_is_wanted(
        mapping.as_ref(),
        &crate::peer::reach::global_v6_addresses(),
    )
}

/// Ask every friend that may carry for this node to hold a carrier, and hand
/// back the channel that stops them again.
///
/// [`None`] when there is nobody to ask, which is not a failure: a Mac nothing
/// can dial and nobody may carry for can still borrow, and the line says so.
fn park_reverse_carriers(
    peers_path: &std::path::Path,
    node: &tcr_peer_wire::PeerId,
    context: &crate::peer::listener::SessionContext,
) -> Option<watch::Sender<bool>> {
    let store = match crate::peer::config::PeerStore::open(peers_path) {
        Ok(store) => store,
        Err(err) => {
            tracing::warn!(
                error = %err,
                "peer reverse: the peers file did not read, so no friend is asked to carry"
            );
            return None;
        }
    };
    let friends = crate::peer::tunnel::reverse_carriers(&store, node);
    // A handle either way, including for the Mac with nobody to ask: the
    // supervisor reads its absence as "ask again at the next wake", and
    // saying this line every five seconds for as long as the operator has
    // pinned nobody is not news.
    let (release, _) = watch::channel(false);
    if friends.is_empty() {
        tracing::info!(
            "peer reverse: nothing can dial this Mac and no pinned Mac may carry for it, so a \
             borrow from here is all it can do (`tcr peer allow <peer> carry` on a friend)"
        );
        return Some(release);
    }
    tracing::info!(
        friends = friends.len(),
        "peer reverse: nothing can dial this Mac, so it is asking friends to hold a carrier"
    );
    for friend in friends {
        // `MAX_PARKED_PER_PEER` keepers per friend, not one, because that
        // constant IS the concurrency an undialable Mac gets on a friend's
        // desk and its own doc says so: one parked carrier serves exactly one
        // forward and is then gone, so a single keeper gives this Mac a pool
        // of one and the second of two streams a borrow needs (the lease ask,
        // then the SERVE) arrives to an empty desk. The desk refuses anything
        // past the cap, which is what keeps this number a ceiling rather than
        // a race: a keeper whose park is refused waits `retry` and tries
        // again.
        for _ in 0..crate::peer::tunnel::MAX_PARKED_PER_PEER {
            let friend = friend.clone();
            let context = context.clone();
            let mut stop = release.subscribe();
            tokio::spawn(supervise("peer-reverse-carrier", async move {
                let keeper = crate::peer::tunnel::keep_reverse_carrier(
                    friend,
                    crate::peer::probe::PROBE_INTERVAL,
                    None,
                    |friend: crate::peer::config::PeerRow| {
                        let context = context.clone();
                        async move { crate::peer::listener::park_one_carrier(&friend, &context).await }
                    },
                );
                tokio::select! {
                    _ = keeper => {}
                    _ = stop.changed() => {}
                }
            }));
        }
    }
    Some(release)
}

/// Write this fleet's lendable headroom, per window, into the lender's ledger.
///
/// `Ledger::may_relay` refuses an ABSENT measurement (see `Ledger::headroom`),
/// so this is what makes a booted lender able to serve anything at all. Every
/// window this build knows, because a lease may be granted on any of them and
/// the one that was never noted is the one that refuses.
fn note_owner_headroom(
    manager: &Arc<Manager>,
    ledger: &Arc<std::sync::Mutex<crate::peer::lease::Ledger>>,
) {
    let now = time::OffsetDateTime::now_utc();
    // A poisoned ledger lock is a panic somewhere in a relay, already logged
    // where it happened. Reported rather than propagated: this is a background
    // note, and refusing to serve local traffic over it would be the wrong
    // trade.
    let Ok(mut held) = ledger.lock() else {
        tracing::warn!("peer listener: the lease ledger's lock is poisoned; headroom not noted");
        return;
    };
    for window in [
        tcr_peer_wire::Window::FiveHour,
        tcr_peer_wire::Window::SevenDay,
        tcr_peer_wire::Window::SevenDayOi,
    ] {
        // `All` only. See `boot_peer_listener`'s doc for why there is no
        // per-scope figure yet.
        let fraction = manager.lendable_fraction(&tcr_peer_wire::LendScope::All, window, now);
        held.note_owner_headroom(window, fraction);
    }
}
