//! The office network key: `tcr peer network-key`, and `tcr peer link`, the share link that carries it.

use anyhow::Context as _;
use tcr::peer;

use super::lock_peers_file;
use super::peer_cli;

/// `tcr peer network-key set|join|clear|show`.
///
/// Split out of [`run_peer`]'s match for the same reason `run_peer_find` is: it
/// is a verb with its own sub-verbs, and folding four more arms into an
/// already-long match makes the one that matters harder to find.
pub(super) fn run_peer_network_key(args: peer_cli::PeerNetworkKeyArgs) -> anyhow::Result<()> {
    use tcr::peer;

    let peers_path = args.peers.path();
    let (_lock, mut file) = lock_peers_file(&peers_path)?;

    match args.action {
        peer_cli::PeerNetworkKeyAction::Show => {
            // The value is never printed, only its presence. `set` prints it
            // once at the moment the operator asked for it and nothing prints
            // it again: a key an operator can re-read from a terminal is a key
            // in every scrollback buffer on the Mac.
            println!(
                "peer network-key: {}",
                if file.network_psk().is_some() {
                    "set"
                } else {
                    "not-set"
                }
            );
            Ok(())
        }
        peer_cli::PeerNetworkKeyAction::Set => {
            // A key already here is not something to overwrite on the strength
            // of one word of argv: every Mac still holding it goes dark to this
            // one, and the refusal names how many that is.
            let (replaced, key) = file.mint_network_key(args.replace)?.map_err(|pinned| {
                anyhow::anyhow!(
                    "peer network-key: a key is already set on this Mac, and minting another \
                     cuts off every Mac still holding the old one, {pinned} pinned peer(s) here, \
                     and any Mac whose beacon this one would stop verifying. Pass --replace to \
                     mean it, or `tcr peer network-key clear` first"
                )
            })?;
            peer::config::save(&peers_path, &file)?;
            println!("{}", key.to_paste_string());
            println!(
                "peer network-key: {} file={}, this is the only time it is printed",
                if replaced { "replaced" } else { "set" },
                peers_path.display()
            );
            if replaced {
                println!(
                    "peer network-key: every Mac still holding the OLD key now sees nothing \
                     of this one and cannot knock at it; paste the new one on each"
                );
            }
            println!(
                "peer network-key: paste it on every other Mac with \
                 `tcr peer network-key join --stdin`, or share it as a link \
                 (`tcr peer link`)"
            );
            Ok(())
        }
        peer_cli::PeerNetworkKeyAction::Join => {
            let raw = if args.stdin {
                let mut line = String::new();
                std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut line)
                    .context("peer network-key: could not read the key from standard input")?;
                line
            } else {
                let Some(key) = args.key else {
                    anyhow::bail!(
                        "peer network-key join: give the key as an argument, or pass --stdin \
                         and pipe it in (which keeps it out of `ps` and the shell history)"
                    );
                };
                key
            };
            if raw.trim().is_empty() {
                anyhow::bail!(
                    "peer network-key join: nothing was given (`--stdin` expects the key on \
                     one line)"
                );
            }
            // The refusal names the FIELD and its shape and never the value:
            // a paste that failed to parse is still a live shared secret.
            let key = peer::config::NetworkKey::try_from(raw)
                .map_err(|refusal| anyhow::anyhow!("{refusal}"))
                .context("peer network-key join: that is not 32 base32-encoded bytes")?;
            // Same refusal as `set`, and for the same reason: pasting a second
            // office's key over the first is the commonest way a Mac
            // disappears from its own mesh.
            let replaced = file.set_network_key(key, args.replace).map_err(|pinned| {
                anyhow::anyhow!(
                    "peer network-key: a key is already set on this Mac, and pasting another \
                     cuts off every Mac still holding the old one, {pinned} pinned peer(s) here. \
                     Pass --replace to mean it, or `tcr peer network-key clear` first"
                )
            })?;
            peer::config::save(&peers_path, &file)?;
            println!(
                "peer network-key: {} file={}",
                if replaced { "replaced" } else { "set" },
                peers_path.display()
            );
            Ok(())
        }
        peer_cli::PeerNetworkKeyAction::Clear => {
            if !file.clear_network_key() {
                println!("peer network-key: not-set (nothing to clear)");
                return Ok(());
            }
            peer::config::save(&peers_path, &file)?;
            println!("peer network-key: cleared file={}", peers_path.display());
            println!(
                "peer network-key: this Mac now sees every `tcr` on the network and they see \
                 it, the caps, the mutes, the blocks and the two-phase approval all still \
                 hold, and every pairing still needs you to press accept"
            );
            Ok(())
        }
    }
}

/// `tcr peer link`: print one link to share, which brings another Mac onto this mesh.
pub(super) fn run_peer_link(a: peer_cli::PeerLinkArgs) -> anyhow::Result<()> {
    let peers_path = a.peers.path();
    let file = peer::config::read_or_default(&peers_path)?;
    let Some(network_key) = file.network_key else {
        anyhow::bail!(
            "peer link: this Mac has no network key, and a share link is a network \
             key (`tcr peer network-key set` mints one). Without one there is \
             nothing for a link to carry, and `tcr peer invite` is the headless \
             path on its own"
        );
    };
    let join = if a.invite {
        let store = peer::config::PeerStore::open(&peers_path)?;
        let label = a.label.clone().unwrap_or_else(|| "joining-mac".to_string());
        let (_invite, token) =
            peer::pair::mint_invite(&store, &label, peer::pair::INVITE_DEFAULT_TTL_SECS, 1, None)?;
        Some(token)
    } else {
        None
    };
    let link = peer::pair::ShareLink { network_key, join };
    println!("{}", link.to_link());
    if a.invite {
        println!(
            "peer link: that link carries a ONE-USE join key good for {} seconds, \
             treat it like a password in a chat window",
            peer::pair::INVITE_DEFAULT_TTL_SECS
        );
    } else {
        println!(
            "peer link: that link carries the network key alone. Opening it lets a \
             Mac see and be seen on this mesh; it pairs nothing and grants nothing"
        );
    }
    Ok(())
}
