//! `tcr peer moved`: mint, or open, the sealed link a Mac sends one friend after it moves.

use std::path::Path;

use anyhow::Context as _;
use tcr::peer;

use super::peer_cli;
use super::{masked_label, now_unix_secs, parse_peer_arg, peer_state_path};

/// `tcr peer moved mint|open`: the sealed link one Mac sends one friend after
/// it changed networks.
///
/// Split out of [`run_peer`]'s match for the reason [`run_peer_network_key`]
/// is: a verb with its own sub-verbs, folded in, makes the arms that matter
/// harder to find.
///
/// # What a link can do to this Mac, and what makes that true
///
/// `open --yes` is the only writing path here, and what it writes goes through
/// [`tcr::peer::discovery::admissible_moved_endpoints`] and then
/// [`tcr::peer::config::observe_endpoints`], through nothing else.
/// That pair is what makes the bound true by construction rather than by
/// review: the second is the peers file's one endpoint writer and answers
/// [`tcr::peer::config::Observed::NoRow`] for a peer it does not
/// already hold, so a link can never pin a Mac, un-forget one, or bring back a
/// row an operator revoked; the first caps
/// how much of a row a link may ever own and leaves every stronger endpoint
/// alone, neither re-dated nor rewritten as a link's.
///
/// Nothing else on a row is reachable from here. No grant, no label, no
/// network key, no switch: `observe_endpoints` writes `PeerRow::endpoints` and
/// there is no second call.
///
/// # Two prefixes, one sentence each
///
/// A line this function wrote itself reads `peer moved: …`, the greppable
/// shape every other verb uses. A refusal that came out of
/// [`tcr::peer::config::moved`] is printed in the module's own words, which
/// start `moved link:`, rather than re-spelled here: one sentence with two
/// spellings is two sentences that drift, and the prefix says which layer
/// refused.
pub(super) fn run_peer_moved(args: peer_cli::PeerMovedArgs) -> anyhow::Result<()> {
    let peers_path = args
        .peers
        .clone()
        .unwrap_or_else(peer::config::default_path);
    match args.action {
        peer_cli::PeerMovedAction::Mint => run_peer_moved_mint(&args, &peers_path),
        peer_cli::PeerMovedAction::Open => run_peer_moved_open(&args, &peers_path),
    }
}

/// `tcr peer moved mint <peer>`: one link for one pinned Mac.
///
/// # Where the addresses come from, and the ones that are left out
///
/// The same ranked set an invite key carries,
/// [`tcr::peer::pair::dial_addresses`], plus the router mapping a
/// serving process holds. The rank is the order a dial should try, and each
/// band is there because a different friend is on the other end: the tailnet
/// address, the mapped external socket, then the addresses a real interface
/// holds.
///
/// The mapping is read off the runtime-state file the way [`run_peer_reach`]
/// reads it, and for the reason that function states: the register
/// [`tcr::peer::reach::external_socket`] answers from is filled by a
/// keeper thread, a CLI process has run none, so asking it here would silently
/// ship the LAN socket alone on a Mac whose mapping is live. A record past its
/// deadline reads as no mapping. It is offered to the ranking AND added
/// afterwards, because `dial_addresses` short circuits on a listen socket
/// somebody pinned by hand and a friend off this LAN would then get a link with
/// nothing in it they can open.
///
/// **The configured listen socket is not itself an address.** `0.0.0.0:7755`
/// is the product default and it is what a wildcard bind honestly reports; it
/// is also the one thing no friend can dial, and a link carrying it teaches a
/// Mac an address that fails every time. Running the listen socket through the
/// same ranking the invite key uses is what turns it into the addresses this
/// Mac actually answers on.
///
/// `PeerRow::sees_us_at`, the address that peer last said it sees this Mac at,
/// is left out **on purpose**. It is written per completed session, and in the
/// case this feature exists for no session has completed since the move, so it
/// holds the address this Mac had BEFORE it moved. A link headed "here is
/// where I am now" carrying it would ship a known wrong answer.
fn run_peer_moved_mint(args: &peer_cli::PeerMovedArgs, peers_path: &Path) -> anyhow::Result<()> {
    if args.stdin {
        anyhow::bail!(
            "peer moved: `--stdin` is how `open` reads a link; `mint` takes the peer id to \
             seal for"
        );
    }
    let Some(target) = args.target.as_deref() else {
        anyhow::bail!(
            "peer moved: `mint` needs the Mac to seal for, in its full wire form, the `node` \
             field of `tcr peer ls --json`"
        );
    };
    let peer_id = parse_peer_arg("peer moved", target)?;

    let file = peer::config::read_or_default(peers_path)?;
    let Some(row) = file.row(&peer_id) else {
        anyhow::bail!(
            "peer moved: that Mac is not pinned here, and a link for one this Mac does not \
             trust would have no key to be sealed under (`tcr peer ls` lists the pinned ones)"
        );
    };
    // The option is handed over whole. There is no default to reach for: a row
    // with no secret is the one case this verb cannot serve, and sealing under
    // thirty-two zero bytes instead would be a key anybody can guess.
    let keys = peer::config::moved::MovedKeys::for_row(row.rendezvous_secret.as_ref())
        .map_err(|refusal| anyhow::anyhow!("{refusal}"))?;

    let now_ms = peer::state::now_ms();
    // Read off the state file a keeper wrote, not off `reach::external_socket`:
    // this process has run no keeper of its own, and the reader both `mint`
    // and `tcr peer invite` share is `peer::reach::recorded_external_socket`.
    let (external, reason) =
        peer::reach::recorded_external_socket(&peer_state_path(peers_path), now_ms);
    // Said out loud rather than skipped: a held mapping this verb could not
    // read is the difference between a link a friend off the LAN can act on
    // and one they cannot.
    if let Some(reason) = reason {
        println!("peer moved: {reason}, so this link carries what this Mac answers on here alone");
    }

    // The interfaces this Mac holds, in the shape the ranking takes them. The
    // live half of `dial_addresses` reads the external socket off a register a
    // keeper thread fills and this process has run none, so the walk is done
    // here and the mapping above is handed in explicitly.
    let host: Vec<peer::pair::HostAddress> = peer::reach::network_fact::interfaces()
        .into_iter()
        .map(|(interface, addr)| peer::pair::HostAddress { interface, addr })
        .collect();
    let ranked = match file.listen {
        Some(listen) => peer::pair::dial_addresses(
            listen,
            &host,
            external,
            peer::reach::tailnet::node::recorded_node_address(&peer_state_path(peers_path), now_ms),
        ),
        None => Vec::new(),
    };

    let mut carried: Vec<std::net::SocketAddr> = Vec::new();
    let mut carry = |addr: std::net::SocketAddr| {
        if !carried.contains(&addr) {
            carried.push(addr);
        }
    };
    for entry in ranked {
        carry(entry.addr);
    }
    if let Some(external) = external {
        carry(external);
    }
    carried.truncate(peer::config::drop::MAX_RECORD_ENDPOINTS);
    if carried.is_empty() {
        anyhow::bail!(
            "peer moved: this Mac has no address to put in a link: nothing it listens on is \
             something a friend could dial, and no serving process holds a router mapping. \
             `tcr peer reach` says what this Mac can be reached on, and `tcr peer internet \
             on` is what asks the router for the second one"
        );
    }

    let config_dir = peers_path
        .parent()
        .map_or_else(peer::config::id::default_config_dir, Path::to_path_buf);
    let this = peer::config::id::NodeKey::load_or_mint(&config_dir)?.id();

    let record = peer::config::moved::MovedRecord {
        v: peer::config::moved::MOVED_VERSION,
        at: now_unix_secs(),
        eps: carried,
    };
    let link = peer::config::moved::mint_link(&keys, &this, &record)
        .map_err(|refusal| anyhow::anyhow!("{refusal}"))?;

    println!("{link}");
    println!(
        "peer moved: sealed for {}; only that Mac can read it, and it goes stale in {} hours",
        masked_label(&row.label),
        peer::config::moved::MAX_MOVED_AGE.as_secs() / 3_600
    );
    println!(
        "peer moved: it carries {} address(es) and nothing else: it joins nothing, grants \
         nothing and pairs nothing",
        record.eps.len()
    );
    Ok(())
}

/// `tcr peer moved open [link]`: read a link somebody sent, and write only when
/// the operator said so.
///
/// # Every pinned row is tried, and they all answer alike
///
/// The link carries no peer id, so the row whose key opens it IS the answer.
/// What that costs is a loop, and the loop's one rule is that its refusal must
/// not depend on the rows it walked: a link forwarded into the wrong group chat
/// must teach its reader nothing about whose Mac it was for or whether this Mac
/// came close. Every key that did not open it answers
/// [`tcr::peer::config::moved::MovedRefusal::NotForThisMac`], which is one
/// sentence naming nothing, and the only refusals that can outrank it are the
/// ones decided after a key DID open the bytes.
fn run_peer_moved_open(args: &peer_cli::PeerMovedArgs, peers_path: &Path) -> anyhow::Result<()> {
    let raw = match (args.stdin, args.target.as_deref()) {
        (true, Some(_)) => anyhow::bail!(
            "peer moved: the link goes on the command line or on standard input, not both"
        ),
        (true, None) => {
            let mut line = String::new();
            std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut line)
                .context("peer moved: could not read the link from standard input")?;
            if line.trim().is_empty() {
                anyhow::bail!(
                    "peer moved: standard input carried no link (`--stdin` expects one line)"
                );
            }
            line
        }
        (false, Some(text)) => {
            // On stderr, so a panel or a script reading stdout sees the same
            // lines whichever way the link arrived.
            eprintln!(
                "peer moved: a link typed on the command line is visible in `ps` to every \
                 process on this Mac and in this shell's history; `--stdin` is the path that \
                 does not leak, and it is the one the panel uses"
            );
            text.to_string()
        }
        (false, None) => anyhow::bail!(
            "peer moved: `open` needs a link, on the command line or on standard input with \
             `--stdin`"
        ),
    };

    let field =
        peer::config::moved::link_field(&raw).map_err(|refusal| anyhow::anyhow!("{refusal}"))?;
    let now_s = now_unix_secs();
    let now_ms = peer::state::now_ms();
    let file = peer::config::read_or_default(peers_path)?;

    // Every pinned row's key is tried in turn; see `PeerFile::open_moved_link`
    // for which refusal wins.
    let (row, record) = match file.open_moved_link(field, now_s) {
        Ok(opened) => opened,
        Err(refusal) => {
            println!(
                "peer moved: nothing was written and no row was created; a link only ever \
                 adds an address to a Mac this one already pins"
            );
            return Err(anyhow::anyhow!("{refusal}"));
        }
    };

    let label = masked_label(&row.label);
    println!(
        "peer moved: from {label} ({}), sealed {}s ago",
        row.node.display(),
        now_s.saturating_sub(record.at)
    );

    let learned: Vec<peer::config::Endpoint> = record
        .eps
        .iter()
        .map(|addr| {
            peer::config::Endpoint::direct(*addr, now_ms, peer::config::EndpointSource::Moved)
        })
        .collect();
    let admissible = peer::discovery::admissible_moved_endpoints(row, &learned);
    // What is already on the row is dropped here rather than refreshed, which
    // is the whole of a second paste of one link costing nothing: the admission
    // rules refresh a locator this band already holds, and refreshing rewrites
    // the operator's file to move one timestamp. A person pasting the same link
    // twice is telling this Mac what it already knows.
    let fresh: Vec<peer::config::Endpoint> = admissible
        .into_iter()
        .filter(|endpoint| {
            !row.endpoints
                .iter()
                .any(|held| held.locator == endpoint.locator)
        })
        .collect();

    if fresh.is_empty() {
        let all_held = learned.iter().all(|endpoint| {
            row.endpoints
                .iter()
                .any(|held| held.locator == endpoint.locator)
        });
        if all_held {
            println!(
                "peer moved: already-known; that Mac is already reachable at everything this \
                 link says, so nothing was written"
            );
        } else {
            println!(
                "peer moved: nothing written; this row already holds the {} addresses a link \
                 may ever add to one, and the rest of its slots are for what this Mac proved",
                peer::discovery::MAX_MOVED_ENDPOINTS_PER_PEER
            );
        }
        return Ok(());
    }

    if !args.yes {
        for endpoint in &fresh {
            match endpoint.direct_addr() {
                Some(addr) => println!("peer moved: would add {addr}"),
                None => println!("peer moved: would add a path that is not a socket"),
            }
        }
        println!("peer moved: nothing written; pass --yes to keep these");
        return Ok(());
    }

    let node = row.node;
    // Three outcomes and three sentences. The writer used to answer one
    // `false` for two different facts, and this arm picked the second of them
    // to print, so a link whose addresses were all undialable told a person
    // their own Mac had forgotten a row the preview had just named.
    match peer::config::observe_endpoints(peers_path, &node, &fresh)? {
        peer::config::Observed::Written { added } => {
            println!("peer moved: added {added} address(es) to {label}; nothing else changed");
        }
        peer::config::Observed::NothingDialable => println!(
            "peer moved: nothing was written; every address in this link is one nothing can \
             dial, an unspecified host or port zero, so {label} is still reachable at \
             whatever its row already held and the Mac that sent this needs to mint a new one"
        ),
        peer::config::Observed::NoRow => println!(
            "peer moved: that Mac has no row here now, so nothing was written; a link never \
             creates one"
        ),
    }
    Ok(())
}
