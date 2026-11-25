//! `tcr peer tailnet`: show or set how this Mac uses a tailnet.

use std::path::PathBuf;

use anyhow::Context as _;
use tcr::peer;

use super::peer_cli;
use super::{listen_written_lines, lock_peers_file, masked_label, print_listen_written};

/// `tcr peer tailnet`: `leave` and a joining `node --auth-key -` are the two
/// async verbs (they wait on the tailnet or on the running tcr); everything
/// else is [`run_peer_tailnet_with`].
pub(super) async fn run_peer_tailnet(args: peer_cli::PeerTailnetArgs) -> anyhow::Result<()> {
    use peer::tailnet_node;

    let peers_path = args.peers.path();
    if args.mode != Some(peer_cli::PeerTailnetMode::Node)
        && (args.auth_key.is_some() || args.name.is_some())
    {
        anyhow::bail!("peer tailnet: --name and --auth-key go with `node` only");
    }
    // A name is asked for when joining; turning an already-joined node back
    // on keeps the name it joined with, so a `--name` there would be ignored.
    if args.name.is_some() && args.auth_key.is_none() {
        anyhow::bail!(
            "peer tailnet: --name is the name to join with, so it goes with `--auth-key -`; a \
             node that already joined keeps its name (`tcr peer tailnet leave` to join again \
             under another)"
        );
    }
    match (args.mode, args.auth_key.as_deref()) {
        (Some(peer_cli::PeerTailnetMode::Leave), _) => {
            let outcome = tailnet_node::leave_with(
                &peers_path,
                &tailnet_node::key_path_for_peers(&peers_path),
                tailnet_node::LEAVE_WAIT,
                tailnet_node::LEAVE_POLL,
            )
            .await?;
            println!("peer.tailnet: off");
            match outcome {
                tailnet_node::LeaveOutcome::Deleted => {
                    println!("peer tailnet: the node's key file was deleted");
                }
                tailnet_node::LeaveOutcome::NoKeyFile => {
                    println!("peer tailnet: there was no node key file to delete");
                }
            }
            println!("peer tailnet: {}", tailnet_node::LEAVE_ADMIN_CONSOLE_NOTE);
            Ok(())
        }
        (Some(peer_cli::PeerTailnetMode::Node), Some("-")) => {
            if !tailnet_node::SUPPORTED {
                anyhow::bail!(node_unsupported_refusal());
            }
            let mut line = String::new();
            std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut line)
                .context("peer tailnet: could not read the auth key from standard input")?;
            let auth_key = tailnet_node::AuthKey::new(&line)?;
            drop(line);
            let me = tailnet_node::join(&peers_path, args.name, auth_key).await?;
            println!("peer.tailnet: node");
            println!("tailnet address: {}", me.address);
            println!("tailnet name: {}", me.name);
            println!(
                "peer tailnet: the running tcr starts the node within {}s",
                (tailnet_node::TICK * 2).as_secs()
            );
            Ok(())
        }
        // The value is not echoed: it is most likely the key itself.
        (Some(peer_cli::PeerTailnetMode::Node), Some(_)) => anyhow::bail!(
            "peer tailnet: --auth-key takes `-` and reads the key from standard input, never \
             the key itself, which would sit in your shell history and in `ps`"
        ),
        _ => run_peer_tailnet_with(args, &peer::tailscale_app::default_cli_paths()),
    }
}

/// The refusal a binary without tcr's own tailnet node gives for `node`.
const fn node_unsupported_refusal() -> &'static str {
    "peer tailnet: this tcr was built without tcr's own tailnet node, so `node` cannot be set; \
     use `tcr peer tailnet app` with the Tailscale app, or build tcr from source with \
     `--features tailnet-node` on an Apple-silicon Mac"
}

/// `tcr peer tailnet [off|app|node] [--json]`, with the Tailscale CLI looked
/// for at `cli_paths` so a test can hand it a fake one.
///
/// Setting a mode takes the peers-file lock and saves, the way
/// [`run_peer_find_with`] writes `discovery`. `node` is refused, with the file
/// untouched, by a binary that cannot run tcr's own tailnet node
/// ([`peer::tailnet_node::SUPPORTED`]), and by one that has not joined yet.
///
/// With no mode it prints the setting and, in app mode, one live read of the
/// app. `--json` prints the setting, whether this binary can act on `node`,
/// and what the serving process recorded (`held`), and never runs the app.
fn run_peer_tailnet_with(
    args: peer_cli::PeerTailnetArgs,
    cli_paths: &[PathBuf],
) -> anyhow::Result<()> {
    use peer::config::TailnetMode;

    let peers_path = args.peers.path();
    if let Some(wanted) = args.mode {
        let Some(mode) = wanted.mode() else {
            anyhow::bail!("peer tailnet: `leave` is run by `run_peer_tailnet`");
        };
        if mode == TailnetMode::Node && !peer::tailnet_node::SUPPORTED {
            anyhow::bail!(node_unsupported_refusal());
        }
        // `node` with no auth key turns a node that already joined back on.
        // With no key file there is nothing for the server to start, so it
        // is refused rather than written as a mode that can only ever say
        // "not joined".
        if mode == TailnetMode::Node
            && peer::tailnet_node::read_key_file(&peer::tailnet_node::key_path_for_peers(
                &peers_path,
            ))?
            .is_none()
        {
            anyhow::bail!(
                "peer tailnet: this Mac has not joined a tailnet as tcr's own node yet; join \
                 with `tcr peer tailnet node --name <name> --auth-key -` and the key on \
                 standard input"
            );
        }
        let listen_written = {
            let (_lock, mut file) = lock_peers_file(&peers_path)?;
            // App and node open a peer port when there is none; off never
            // writes `listen` (see `PeerFile::switch_tailnet`).
            let written = file.switch_tailnet(mode);
            peer::config::save(&peers_path, &file)?;
            written
        };
        if !args.json {
            println!("peer.tailnet: {}", mode.as_str());
            if let Some(chosen) = listen_written {
                print_listen_written(chosen, &peers_path);
            }
            return Ok(());
        }
        // Standard output is the JSON document, so the port lines go to
        // standard error rather than being left unsaid.
        if let Some(chosen) = listen_written {
            for line in listen_written_lines(chosen, &peers_path) {
                eprintln!("{line}");
            }
        }
    }

    let report = peer::tailnet_node::TailnetReport::read(&peers_path, peer::state::now_ms())?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    println!("peer.tailnet: {}", report.mode.as_word());
    if let peer::config::TailnetSetting::Unknown(word) = &report.mode {
        // The surfacing for an unknown word: it loads and acts as off, and
        // this line is where the operator learns that.
        println!(
            "peer.tailnet: \"{}\" is a mode this build of tcr does not know, so it is treated \
             as off; a newer tcr probably wrote it",
            masked_label(word)
        );
    }
    let mode = report.mode.effective();
    if mode == TailnetMode::App {
        print_tailscale_app_read(cli_paths);
    }
    if mode == TailnetMode::Node && !report.node_supported {
        println!("peer.tailnet: this tcr was built without tcr's own tailnet node");
    }
    if mode == TailnetMode::Node && report.node_supported {
        match report
            .held
            .as_ref()
            .filter(|held| !held.is_stale(peer::state::now_ms()))
        {
            Some(held) => {
                if let Some(address) = held.address {
                    println!("tailnet address: {address}");
                }
                if let Some(name) = &held.node_name {
                    println!("tailnet name: {name}");
                }
                println!("tailnet node: {:?}", held.state);
                if let Some(detail) = &held.detail {
                    println!("tailnet node: {detail}");
                }
            }
            None => println!("tailnet node: no running tcr has recorded the node yet"),
        }
    }
    Ok(())
}

/// One live read of the Tailscale app, as lines for `tcr peer tailnet`.
fn print_tailscale_app_read(cli_paths: &[PathBuf]) {
    use peer::tailscale_app::{self, Route};

    match tailscale_app::read_status(cli_paths, tailscale_app::STATUS_TIMEOUT) {
        Ok(status) => {
            match status.own_ipv4() {
                Some(address) => println!("tailnet address: {address}"),
                None => println!("tailnet address: none reported"),
            }
            let mine = tailscale_app::own_macs(&status);
            if mine.is_empty() {
                println!("your other Macs online: none");
            }
            for mac in mine {
                let address = mac
                    .tailscale_ips
                    .iter()
                    .find(|ip| ip.is_ipv4())
                    .or_else(|| mac.tailscale_ips.first())
                    .map_or_else(|| "no address".to_string(), ToString::to_string);
                let route = match tailscale_app::route(mac) {
                    Route::Direct => "direct",
                    Route::Relayed => "relayed",
                    Route::Unknown => "route unknown",
                };
                println!(
                    "your Mac: {}  {address}  {route}",
                    masked_label(&mac.host_name)
                );
            }
        }
        Err(err) => println!("tailscale app: {}", err.detail()),
    }
}

#[cfg(test)]
mod peer_tailnet_tests {
    use super::*;
    use std::path::Path;

    fn args(
        peers_path: &Path,
        mode: Option<peer_cli::PeerTailnetMode>,
    ) -> peer_cli::PeerTailnetArgs {
        peer_cli::PeerTailnetArgs {
            peers: peer_cli::PeersFile {
                peers: Some(peers_path.to_path_buf()),
            },
            mode,
            name: None,
            auth_key: None,
            json: false,
        }
    }

    fn written(peers_path: &Path) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(peers_path).expect("the peers file"))
            .expect("the peers file is JSON")
    }

    /// **`tcr peer tailnet app` writes `"tailnet": "app"`, keeps every other
    /// key, and `off` takes the key away again.**
    ///
    /// Watch it fail by dropping the `file.tailnet = mode;` line: the file
    /// then never gains the key.
    #[test]
    fn setting_a_mode_writes_the_key_and_keeps_the_rest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let peers_path = dir.path().join("tcr-peers.json");
        let seed = peer::config::PeerFile {
            discovery: true,
            ..Default::default()
        };
        peer::config::save(&peers_path, &seed).expect("seed the peers file");

        run_peer_tailnet_with(args(&peers_path, Some(peer_cli::PeerTailnetMode::App)), &[])
            .expect("set app");
        let file = written(&peers_path);
        assert_eq!(file["tailnet"], "app", "{file}");
        assert_eq!(
            file["discovery"], true,
            "the rest of the file is kept: {file}"
        );

        run_peer_tailnet_with(args(&peers_path, Some(peer_cli::PeerTailnetMode::Off)), &[])
            .expect("set off");
        let file = written(&peers_path);
        assert!(
            file.get("tailnet").is_none(),
            "off is the default and is not written: {file}"
        );
    }

    /// **`node` is refused by a build without the feature, and the file is
    /// left as it was.**
    ///
    /// Watch it fail by removing the feature check: the call then succeeds
    /// and the file says `node`.
    #[cfg(not(all(feature = "tailnet-node", target_os = "macos", target_arch = "aarch64")))]
    #[test]
    fn node_is_refused_without_the_feature() {
        let dir = tempfile::tempdir().expect("tempdir");
        let peers_path = dir.path().join("tcr-peers.json");
        run_peer_tailnet_with(args(&peers_path, Some(peer_cli::PeerTailnetMode::App)), &[])
            .expect("set app");

        let refusal = run_peer_tailnet_with(
            args(&peers_path, Some(peer_cli::PeerTailnetMode::Node)),
            &[],
        )
        .expect_err("node needs the tailnet-node feature")
        .to_string();
        assert!(refusal.contains("tailnet-node"), "{refusal}");
        assert_eq!(
            written(&peers_path)["tailnet"],
            "app",
            "a refused mode leaves the file alone"
        );
    }

    /// **The no-argument form prints in every mode without failing, including
    /// app mode with no Tailscale CLI anywhere.** A missing app is a line of
    /// output, never an error exit.
    #[test]
    fn showing_the_mode_with_no_app_installed_is_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let peers_path = dir.path().join("tcr-peers.json");
        run_peer_tailnet_with(args(&peers_path, None), &[]).expect("show off");
        run_peer_tailnet_with(args(&peers_path, Some(peer_cli::PeerTailnetMode::App)), &[])
            .expect("set app");
        run_peer_tailnet_with(args(&peers_path, None), &[dir.path().join("no-cli")])
            .expect("show app with no CLI");
    }

    /// **A mode word this build does not know is shown, not refused, and
    /// setting a known mode replaces it.**
    #[test]
    fn an_unknown_mode_is_shown_and_can_be_replaced() {
        let dir = tempfile::tempdir().expect("tempdir");
        let peers_path = dir.path().join("tcr-peers.json");
        std::fs::write(&peers_path, r#"{"tailnet": "both"}"#).expect("write the peers file");
        std::fs::set_permissions(
            &peers_path,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o600),
        )
        .expect("mode 0600, which the reader requires");
        run_peer_tailnet_with(args(&peers_path, None), &[]).expect("show an unknown mode");
        assert_eq!(
            written(&peers_path)["tailnet"],
            "both",
            "showing writes nothing"
        );
        run_peer_tailnet_with(args(&peers_path, Some(peer_cli::PeerTailnetMode::App)), &[])
            .expect("set app over it");
        assert_eq!(written(&peers_path)["tailnet"], "app");
    }

    /// **`app` gives a Mac with no peer port the default one, the way `find
    /// on` does; `off` never writes one and never removes one; a port that is
    /// already set is kept.** The browse task that reads the app and sends
    /// the beacon runs only beside a peer listener, so `app` without a port
    /// found nothing at all.
    ///
    /// Watch it fail by dropping the `ensure_listen_for_opt_in` call from the
    /// `App | Node` arm in `PeerFile::switch_tailnet`: the file then has no
    /// `listen` after `app`.
    #[test]
    fn app_gives_a_mac_a_peer_port_and_off_never_touches_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let peers_path = dir.path().join("tcr-peers.json");
        let set = |mode| {
            run_peer_tailnet_with(args(&peers_path, Some(mode)), &[]).expect("set the mode");
        };

        set(peer_cli::PeerTailnetMode::Off);
        assert!(
            written(&peers_path).get("listen").is_none(),
            "off is not an opt-in: {}",
            written(&peers_path)
        );

        set(peer_cli::PeerTailnetMode::App);
        assert_eq!(
            written(&peers_path)["listen"],
            peer::config::default_listen().to_string()
        );

        set(peer_cli::PeerTailnetMode::Off);
        assert_eq!(
            written(&peers_path)["listen"],
            peer::config::default_listen().to_string(),
            "off leaves the port it found"
        );

        let mut file = peer::config::read_or_default(&peers_path).expect("read");
        file.listen = Some("127.0.0.1:9600".parse().expect("literal"));
        peer::config::save(&peers_path, &file).expect("save");
        set(peer_cli::PeerTailnetMode::App);
        assert_eq!(
            written(&peers_path)["listen"],
            "127.0.0.1:9600",
            "a port the operator chose is kept"
        );
    }
}
