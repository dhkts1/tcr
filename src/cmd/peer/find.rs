//! `tcr peer find`: find other Macs on this network, and let them find this one.

use tcr::peer;

use super::peer_cli;
use super::{lock_peers_file, masked_label, print_listen_written};

/// `tcr peer find on|off [--announce-name on|off]`, one switch that starts or
/// stops both [`peer::discovery::advertise`] and [`peer::discovery::browse`].
///
/// An earlier version of this function wrote its own
/// `tcr-peers.json` read/write and its own default-path/default-name
/// fallbacks because `PeerStore::open`, `peer::config::default_path` and
/// `PeerFile::display_name` were `todo!()` at the time. Phase 1 landed, so
/// this now sits entirely on [`peer::config`]'s real API: no second reader,
/// no second writer, no second host-name fallback.
pub(super) async fn run_peer_find(args: peer_cli::PeerFindArgs) -> anyhow::Result<()> {
    run_peer_find_with(args, &RealPeerFinder).await
}

/// The one "start" call `find on` makes from the CLI: begin browsing.
///
/// A trait so [`find_on_starts_a_browse`] below can prove it fired without a
/// real mDNS daemon behind it, [`RealPeerFinder`] is the only production
/// implementation and just delegates to [`peer::discovery::browse`].
///
/// **Announcing is NOT here any more.** This command used to register the
/// beacon in this process and exit, which took the mdns-sd daemon, and the
/// beacon, down with the command; and the beacon carried this process's
/// per-boot instance id, which the server's own knocks could never match. The
/// server announces (`server::spawn_beacon`), and what this command does is
/// write the flag the server reads.
trait PeerFinder {
    async fn start_browse(&self, store: &peer::config::PeerStore) -> anyhow::Result<()>;
}

/// The real mesh.
struct RealPeerFinder;

impl PeerFinder for RealPeerFinder {
    async fn start_browse(&self, store: &peer::config::PeerStore) -> anyhow::Result<()> {
        // `find on` runs one scan and prints what it saw, then writes
        // nothing: this CLI process's mDNS daemon dies with the command, so
        // a scan here is a diagnostic readout for an operator at a prompt
        // and not a source of truth. The list `tcr peer ls --json` and the
        // panel read comes from the serving process's own browse task
        // (`server::spawn_browse`), which writes into `peer-state.json` on
        // its own cadence; a second writer on that key is the defect the
        // peers-file lock exists to prevent.
        let found = peer::discovery::browse(store).await?;
        if found.is_empty() {
            println!("no Macs found");
        }
        for row in &found {
            // Reusing `Found::dial_address` rather than a second bracket
            // rule: the timestamps are unused by that method and are not
            // written anywhere, this row is never persisted.
            let as_found = row.found_row(0);
            let name = as_found
                .announced_name
                .as_deref()
                .map_or_else(|| "no name announced".to_string(), masked_label);
            println!("{}  {}", as_found.dial_address(), name);
        }
        Ok(())
    }
}

async fn run_peer_find_with(
    args: peer_cli::PeerFindArgs,
    finder: &impl PeerFinder,
) -> anyhow::Result<()> {
    let peers_path = args.peers.path();
    let (_lock, mut file) = lock_peers_file(&peers_path)?;

    // The flag flips the config key on BOTH arms (see `PeerFile::switch_find`):
    // it is a stored preference and does not depend on which way discovery is
    // being set.
    let announce_name = args
        .announce_name
        .map(|state| matches!(state, peer_cli::Switch::On));

    // What this run wrote into `listen`, if anything. Said after the save and
    // not before it, because a scan that fails exits non-zero having written
    // nothing, and a line promising a port in that case would be a lie.
    //
    // This used to refuse on the `on` arm and send the operator to a text
    // editor for a key no walkthrough names, which stopped a first-time
    // reader at their very first command. Finding peers needs a port to
    // announce, so the verb provides one instead of asking for it.
    let listen_written =
        file.switch_find(matches!(args.state, peer_cli::Switch::On), announce_name);

    match args.state {
        peer_cli::Switch::Off => {
            // The flag, and nothing else: the beacon lives in the serving
            // process, which reads this file and withdraws it. A `stop_all`
            // here would reach only this command's own (empty) daemon.
            println!("peer.find: off");
            println!("a running server stops announcing within a minute");
        }
        peer_cli::Switch::On => {
            let store = peer::config::PeerStore::open(&peers_path)?;
            // One scan, so `find on` reports what is already on the LAN. The
            // ANNOUNCING half is the server's: see [`PeerFinder`].
            finder.start_browse(&store).await?;
            println!("peer.find: on (announceName={})", file.announce_name);
            // Only when there was already a port. A server that is running
            // right now has no peer socket for one this command just wrote, so
            // promising it would announce within a minute is exactly the claim
            // `print_listen_written` exists to correct.
            if listen_written.is_none() {
                println!("a running server starts announcing within a minute");
            }
        }
    }

    peer::config::save(&peers_path, &file)?;
    if let Some(chosen) = listen_written {
        print_listen_written(chosen, &peers_path);
    }
    Ok(())
}

#[cfg(test)]
mod peer_find_tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    /// Records that its call happened; no real mDNS daemon, no real `browse`.
    #[derive(Default)]
    struct FakePeerFinder {
        browsed: AtomicBool,
    }

    impl PeerFinder for FakePeerFinder {
        async fn start_browse(&self, _store: &peer::config::PeerStore) -> anyhow::Result<()> {
            self.browsed.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    /// `find on` writes the flag and scans once, and announces NOTHING from
    /// this process: the beacon belongs to the server, which is the only
    /// process that outlives the command and the only one whose instance id a
    /// neighbour's knock can name.
    ///
    /// Watch it fail: comment out the `finder.start_browse(&store).await?;`
    /// line above and this goes red on the browse assertion, confirmed by
    /// hand while writing this test.
    #[tokio::test]
    async fn find_on_writes_the_flag_and_scans_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let peers_path = dir.path().join("tcr-peers.json");
        let seed = peer::config::PeerFile {
            listen: Some("127.0.0.1:0".parse().expect("valid socket addr")),
            ..Default::default()
        };
        peer::config::save(&peers_path, &seed).expect("seed peers file");

        let finder = FakePeerFinder::default();
        run_peer_find_with(
            peer_cli::PeerFindArgs {
                peers: peer_cli::PeersFile {
                    peers: Some(peers_path.clone()),
                },
                state: peer_cli::Switch::On,
                announce_name: None,
            },
            &finder,
        )
        .await
        .expect("run_peer_find_with(on)");

        assert!(
            finder.browsed.load(Ordering::SeqCst),
            "find on must scan once"
        );
        assert!(
            peer::config::read_or_default(&peers_path)
                .expect("the peers file reads back")
                .discovery,
            "and it must write the flag the server reads, which is the whole of what turns \
             announcing on"
        );
    }

    /// `find off` starts neither: the default state, and the one documented
    /// as "peer.find off (the default) registers nothing".
    #[tokio::test]
    async fn find_off_starts_neither() {
        let dir = tempfile::tempdir().expect("tempdir");
        let peers_path = dir.path().join("tcr-peers.json");

        let finder = FakePeerFinder::default();
        run_peer_find_with(
            peer_cli::PeerFindArgs {
                peers: peer_cli::PeersFile {
                    peers: Some(peers_path),
                },
                state: peer_cli::Switch::Off,
                announce_name: None,
            },
            &finder,
        )
        .await
        .expect("run_peer_find_with(off)");

        assert!(
            !finder.browsed.load(Ordering::SeqCst),
            "find off must not browse"
        );
    }
}
