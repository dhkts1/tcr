//! Where requests leave from: `tcr peer via` for this Mac, and `tcr peer account` for one account's exit lock.

use tcr::base::config;
use tcr::peer;
use tcr::verbs::cli;

use super::peer_cli;

/// `tcr peer via`: choose how this Mac reaches the internet.
pub(super) fn run_peer_via(a: peer_cli::PeerViaArgs) -> anyhow::Result<()> {
    // `auto`, `off`, or one pinned Mac. The word is parsed here so a
    // typo is refused before anything else happens, and a peer id is
    // checked against the pinned rows so `via <peer>` cannot name a
    // Mac this one has never trusted.
    let peers_path = a.peers.path();
    // The lock is taken BEFORE the read, because this is a
    // read-modify-write of the same file `tcr peer allow` and the
    // listener's enrolment write also round-trip: without it the last
    // writer wins and one of the two operators' choices disappears.
    let lock = peer::config::FileLock::acquire(&peers_path)?;
    let mut file = peer::config::read_or_default(&peers_path)?;
    let asked = match a.setup_timeout_ms {
        None => peer::config::ViaSetting::parse(&a.target)?,
        Some(ms) => peer::config::ViaSetting::parse(&a.target)?.with_setup_timeout_ms(ms)?,
    };
    // The whole write: one field on the struct every other `tcr peer`
    // mutation round-trips, so the choice survives the next `allow`,
    // `lend` or `name` instead of being dropped by it.
    if !file.choose_via(&asked)? {
        println!("peer via: {} (unchanged)", asked.to_spec());
        println!(
            "peer via: `auto` carries a request through a trusted Mac only after the \
             direct path has failed with nothing having left this Mac, and the carry \
             is blind, the credential stays inside this Mac's own TLS"
        );
        return Ok(());
    }
    peer::config::save(&peers_path, &file)?;
    drop(lock);
    println!("peer via: ok {}", asked.to_spec());
    if let Some(ms) = a.setup_timeout_ms {
        println!("peer via: a carry gives a Mac {ms}ms to take it, then gives up");
    }
    if asked.is_off() {
        println!(
            "peer via: off, this Mac never routes out through a peer, and a request \
             whose direct path fails gets exactly the answer it got before this \
             feature existed"
        );
    } else if let Some(peer_id) = asked.pinned_peer() {
        println!(
            "peer via: only {} is asked, and never any other Mac, a pin that stops \
             answering means no carry at all, which is the point of pinning one",
            peer_id.display()
        );
    } else {
        println!(
            "peer via: auto carries a request through a trusted Mac only after the \
             direct path has failed with nothing having left this Mac, and the carry \
             is blind, the credential stays inside this Mac's own TLS"
        );
    }
    println!("peer via: it takes effect on the next request; nothing needs a restart");
    Ok(())
}

/// `tcr peer account <query> --exits-from local|<peer> [--must|--no-must]`:
/// write one account's exit lock.
///
/// The write goes through `cli::set_account_egress`, which is `edit_account`,
/// which is the chain `tcr disable` and `tcr control` already use: one
/// resolution of the account query, one ambiguous-query message, no partial
/// write. This function is argv, a name lookup and the printed line.
///
/// # A NAME is resolved here and a peer id is not
///
/// `--exits-from` takes either, because an operator reads names off
/// `tcr peer ls` and ids are 52 characters. A name is matched against the
/// pinned rows and must match exactly one; a value that parses as a peer id is
/// taken as one WITHOUT checking it is pinned, which is deliberate: pinning an
/// account to a Mac that is not trusted yet is an order the operator may give
/// before pairing, and the request path refuses it on its own terms (an
/// unreachable exit falls back, or refuses under `--must`). A name that
/// matches nothing cannot be that, because there is nothing to have meant.
pub(super) fn run_peer_account(a: peer_cli::PeerAccountArgs) -> anyhow::Result<()> {
    let config_path = a.config.unwrap_or_else(config::default_path);
    let egress = match a.exits_from.as_deref() {
        None => None,
        Some(raw) if raw.trim() == config::Egress::LOCAL => Some(config::Egress::Local),
        Some(raw) => {
            let raw = raw.trim();
            if let Ok(peer) = tcr_peer_wire::PeerId::parse(raw) {
                Some(config::Egress::Via(peer))
            } else {
                let peers_path = a.peers.unwrap_or_else(peer::config::default_path);
                let store = peer::config::PeerStore::open(&peers_path)?;
                let matched: Vec<peer::config::PeerRow> = store
                    .peers()
                    .into_iter()
                    .filter(|row| row.label == raw)
                    .collect();
                match matched.as_slice() {
                    [only] => Some(config::Egress::Via(only.node)),
                    [] => anyhow::bail!(
                        "no pinned Mac is named `{raw}`, and it is not a peer id either \
                         (`tcr peer ls` prints both, and `local` is the word for this \
                         Mac's own socket)"
                    ),
                    several => anyhow::bail!(
                        "`{raw}` names {} pinned Macs, so it cannot say which one this \
                         account leaves from; give the peer id instead (`tcr peer ls \
                         --json` prints it as `node`)",
                        several.len()
                    ),
                }
            }
        }
    };
    let strict = match (a.must, a.no_must) {
        (true, _) => Some(true),
        (_, true) => Some(false),
        _ => None,
    };
    if egress.is_none() && strict.is_none() {
        anyhow::bail!(
            "nothing to change: give --exits-from, --must or --no-must (`tcr peer ls --json` \
             prints every account that has an exit lock under `exits`)"
        );
    }
    let (name, pin) = cli::set_account_egress(&config_path, &a.account, egress, strict)?;
    println!(
        "peer account {name}: exits from {}{}",
        pin.egress,
        if pin.strict {
            ", and a request it cannot carry is refused rather than sent from this Mac"
        } else {
            ", falling back to this Mac when that exit is unavailable"
        }
    );
    println!(
        "the exit lock is read at boot, so a proxy already running keeps the old one until it \
         restarts"
    );
    Ok(())
}
