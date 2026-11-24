//! The dead drop: `tcr peer drop` turns it on or off, `tcr peer drop-store` points it at an HTTPS surface.

use anyhow::Context as _;
use tcr::peer;

use super::lock_peers_file;
use super::peer_cli;

/// `tcr peer drop`: turn this Mac's dead drop on or off.
pub(super) fn run_peer_drop(a: peer_cli::PeerDropArgs) -> anyhow::Result<()> {
    let peers_path = a.peers.unwrap_or_else(peer::config::default_path);
    let (_lock, mut file) = lock_peers_file(&peers_path)?;
    let on = matches!(a.state, peer_cli::Switch::On);
    // Same shape `tcr peer find on` uses when `listen` is absent: say
    // what to run first and change nothing, rather than writing a
    // switch that starts no task because `DeadDropConfig::is_live`
    // needs a store too.
    if on && file.dead_drop.store.is_none() {
        println!(
            "peer.drop: not turned on, no store is configured yet; run \
             `tcr peer drop-store set <url>` first, or `tcr peer drop-store set \
             https://paste.example/{{name}}` for the shape it expects"
        );
        return Ok(());
    }
    file.dead_drop.enabled = on;
    peer::config::save(&peers_path, &file)?;
    println!("peer.drop: {}", if on { "on" } else { "off" });
    if on {
        println!(
            "peer.drop: publishes and fetches only for peers with `control-drop` \
             granted (`tcr peer allow <peer> control-drop on`, both Macs); the running \
             proxy re-reads this file whenever its mtime moves, so no restart is needed"
        );
    }
    Ok(())
}

/// `tcr peer drop-store`: point the dead drop at an HTTPS surface, or show or clear it.
pub(super) fn run_peer_drop_store(a: peer_cli::PeerDropStoreArgs) -> anyhow::Result<()> {
    let peers_path = a.peers.unwrap_or_else(peer::config::default_path);
    let (_lock, mut file) = lock_peers_file(&peers_path)?;
    match a.action {
        peer_cli::PeerDropStoreAction::Show => {
            match &file.dead_drop.store {
                Some(peer::config::StoreConfig::Https { url, token }) => println!(
                    "peer drop-store: https {url} token={}",
                    if token.is_some() { "set" } else { "not-set" }
                ),
                Some(peer::config::StoreConfig::Gist { gist_id, .. }) => println!(
                    "peer drop-store: gist {gist_id} (this build does not ship a gist \
                     backend; `tcr peer drop on` refuses with not-configured)"
                ),
                None => println!("peer drop-store: not-set"),
            }
            Ok(())
        }
        peer_cli::PeerDropStoreAction::Clear => {
            let had = file.dead_drop.store.take().is_some();
            peer::config::save(&peers_path, &file)?;
            println!(
                "peer drop-store: {}",
                if had {
                    "cleared"
                } else {
                    "was already not-set"
                }
            );
            Ok(())
        }
        peer_cli::PeerDropStoreAction::Set => {
            let Some(value) = a.value.clone() else {
                anyhow::bail!(
                    "peer drop-store set: give the url template as an argument, \
                     containing {{name}}, for example \
                     `tcr peer drop-store set https://paste.example/{{name}}`"
                );
            };
            if value.starts_with("gist:") {
                anyhow::bail!(
                    "peer drop-store set: this build does not ship a gist backend, \
                     use an https template instead"
                );
            }
            let token = match a.token {
                None => None,
                Some(Some(given)) => Some(given),
                Some(None) => {
                    let mut line = String::new();
                    std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut line).context(
                        "peer drop-store set: could not read the token from \
                             standard input",
                    )?;
                    let trimmed = line.trim().to_string();
                    if trimmed.is_empty() {
                        anyhow::bail!(
                            "peer drop-store set: --token was given with nothing on \
                             standard input"
                        );
                    }
                    Some(trimmed)
                }
            };
            // The construction itself is the refusal for a template
            // with no `{name}` placeholder: `DeadDropConfig::open_store`'s
            // own doc says this verb should have refused it first, and
            // building the real client here is one value read by every
            // consumer rather than a second, divergent check.
            peer::drop::HttpsTemplateStore::new(&value, token.as_deref())
                .map_err(|err| anyhow::anyhow!("peer drop-store set: {err}"))?;
            file.dead_drop.store = Some(peer::config::StoreConfig::Https {
                url: value.clone(),
                token,
            });
            peer::config::save(&peers_path, &file)?;
            println!("peer drop-store: set https {value}");
            if file.dead_drop.enabled {
                println!(
                    "peer drop-store: peer.drop is already on, this Mac starts \
                     publishing to it the next time the proxy starts"
                );
            } else {
                println!("peer drop-store: turn it on with `tcr peer drop on`");
            }
            Ok(())
        }
    }
}
