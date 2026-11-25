//! Trusting and naming Macs: `tcr peer name`, `pair`, `invite`, `join`, `forget` and `hello`.

use anyhow::Context as _;

use super::peer_cli;
use super::{lock_peers_file, parse_peer_arg, peer_state_path};

/// `tcr peer name`: set this Mac's display name, what another Mac shows for it.
pub(super) fn run_peer_name(a: peer_cli::PeerNameArgs) -> anyhow::Result<()> {
    let peers_path = a.peers.path();
    // THE LOCK, and the review's M3. `FileLock`'s own doc says it
    // "stops two `tcr` processes racing the same file, which is the real
    // case (a listener enrolling a joiner while the operator runs
    // `tcr peer allow`)", and that sentence was not true: every peers-file
    // mutation in this CLI was a bare read-modify-write, so an operator's
    // revoke and a listener's enrolment each wrote over the other's row,
    // silently, in a window of milliseconds. `accept_enrolment` takes the
    // same lock, which is what makes the pair exclusive.
    let (_lock, mut file) = lock_peers_file(&peers_path)?;

    match a.name {
        None => println!("{}", file.display_name()),
        Some(name) => {
            let sanitized = file.rename(&name)?;
            tcr::peer::config::save(&peers_path, &file)?;
            println!("{sanitized}");
        }
    }
    Ok(())
}

/// `tcr peer pair`: trust a Mac interactively, both screens showing six digits.
pub(super) async fn run_peer_pair(a: peer_cli::PeerPairArgs) -> anyhow::Result<()> {
    use tcr::peer::pair::PairEvent;

    /// Print one `--json` line, or nothing at all in prose mode.
    ///
    /// A function rather than a bare `println!` at four call sites:
    /// every arm below has to be either prose or JSON and never both,
    /// and the one that forgets is the one that puts an English
    /// sentence into a stream a parser is reading.
    fn announce(json: bool, event: &PairEvent) -> anyhow::Result<()> {
        if json {
            println!("{}", event.line()?);
        }
        Ok(())
    }

    let path = a.peers.path();
    let store = tcr::peer::config::PeerStore::open(&path)?;
    // A bare host is taken too, and defaulted: see `peer_dial_addr`.
    // `tcr peer pending` prints a bare address for a knock that named
    // no port, and this command has to accept what that row printed.
    let addr: std::net::SocketAddr = peer_dial_addr(&a.addr)?;
    // **Phase one: knock.** This replaced the direct-`XX` dial.
    // The older `tcr peer pair` opened a
    // two-minute window on THIS Mac and dialled, so any host that
    // could reach this port during those two minutes got message 2 and
    // this node's static key with it. Now nothing is disclosed to
    // anyone until an operator at the far Mac has seen a request and
    // pressed Accept.
    let file = tcr::peer::config::read_or_default(&path)?;
    let proposed_name = file.display_name();
    let instance = tcr::peer::id::boot_instance_id();
    if let Err(err) = tcr::peer::pair::knock(&store, addr, &proposed_name).await {
        announce(
            a.json,
            &PairEvent::Refused {
                message: format!("{err:#}"),
            },
        )?;
        return Err(err);
    }
    announce(
        a.json,
        &PairEvent::Asking {
            addr: addr.to_string(),
            instance: instance.to_string(),
            wait_seconds: tcr::peer::pair::PAIR_WAIT.as_secs(),
        },
    )?;
    if !a.json {
        println!("peer pair: asked {addr} to pair and waiting for them, as instance {instance}");
        println!(
            "peer pair: on that Mac, `tcr peer pending` shows the request and \
             `tcr peer accept {instance}` approves it; nothing has been disclosed to it \
             yet"
        );
    }

    // **Phase two: `XX`, once their Accept opens a window for this
    // instance id.** Retried, because approval can come minutes later.
    // See `pair::PAIR_WAIT`.
    let pending = match tcr::peer::pair::pair(&store, addr).await {
        Ok(pending) => pending,
        Err(err) => {
            // The ten-minute deadline lands here, and it is the one
            // failure a panel MUST be able to draw: it is what "nobody
            // was at the other Mac" looks like.
            announce(
                a.json,
                &PairEvent::Refused {
                    message: format!("{err:#}"),
                },
            )?;
            return Err(err);
        }
    };
    announce(
        a.json,
        &PairEvent::Comparing {
            code: pending.code.clone(),
        },
    )?;
    if !a.json {
        println!("peer pair: this Mac shows {}", pending.code);
        println!(
            "peer pair: the other Mac shows six digits for the same pairing; they must \
             be identical, and a mismatch means a machine in the middle relayed it"
        );
    }
    let offered = if let Some(code) = a.code.clone() {
        code
    } else {
        // The prompt is prose only. In `--json` the `comparing`
        // line above IS the prompt, and a reader that got an
        // English sentence on the same stream would have to
        // decide which of the two to believe.
        if !a.json {
            println!("peer pair: type the digits the other Mac is showing:");
        }
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .context("peer pair: could not read the compared code")?;
        line
    };
    if !pending.matches(&offered) {
        let message = format!(
            "peer pair: refused, {} here, {} there. A mismatch is the one signal \
             this path exists to produce, so it is not a retry prompt",
            pending.code,
            offered.trim()
        );
        announce(
            a.json,
            &PairEvent::Refused {
                message: message.clone(),
            },
        )?;
        anyhow::bail!(message);
    }
    tcr::peer::pair::confirm(&store, &pending.peer, &pending.code, Some(addr))?;
    announce(
        a.json,
        &PairEvent::Trusted {
            peer: pending.peer.display(),
        },
    )?;
    if !a.json {
        println!(
            "peer pair: trusted, and a bare pin can say hello and nothing else \
             (`tcr peer allow` grants one thing at a time, with no restart)"
        );
        println!(
            "peer pair: run the same command on the other Mac, pointed back here, so \
             both sides hold a pin"
        );
    }
    Ok(())
}

/// `tcr peer invite`: mint a one-line join key for a Mac with no screen.
pub(super) async fn run_peer_invite(a: peer_cli::PeerInviteArgs) -> anyhow::Result<()> {
    let path = a.peers.path();
    let store = tcr::peer::config::PeerStore::open(&path)?;
    if let Some(id) = a.revoke {
        let removed = tcr::peer::pair::revoke_invite(&store, id)?;
        println!(
            "peer invite: {} revoke id={id}",
            if removed { "ok" } else { "not-found" }
        );
        return Ok(());
    }
    if a.sealed {
        let (secret, public) = tcr::peer::noise::generate_static()?;
        let now_ms = tcr::peer::state::now_ms();
        let until_ms = now_ms.saturating_add(600_000);
        let id_bytes = tcr::peer::id::random_secret()?;
        let [b0, b1, b2, b3, b4, b5, b6, b7, ..] = id_bytes;
        let id = u64::from_be_bytes([b0, b1, b2, b3, b4, b5, b6, b7]);
        tcr::peer::state::add_ask(
            &peer_state_path(&path),
            tcr::peer::ask::PendingAsk {
                id,
                public,
                private: secret,
                until_ms,
            },
        )?;
        let ask = tcr::peer::ask::Ask { key: public };
        println!("{}", ask.to_string_wire());
        println!(
            "peer invite: this names no address and grants nothing; whoever holds it \
             can send you an address and nothing else"
        );
        println!(
            "peer invite: it is good for ten minutes; paste what comes back with \
             tcr peer invite --reply --stdin"
        );
        return Ok(());
    }
    if a.reply {
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .context("peer invite: could not read the reply from standard input")?;
        if line.trim().is_empty() {
            anyhow::bail!(
                "peer invite: standard input carried no reply (`--reply --stdin` \
                 expects the {}… line on stdin)",
                tcr::peer::ask::REPLY_PREFIX
            );
        }
        let now_ms = tcr::peer::state::now_ms();
        let opened =
            tcr::peer::state::open_and_spend_ask(&peer_state_path(&path), line.trim(), now_ms)?;
        let tcr::peer::ask::Opened::Key(token) = opened;
        // Opening a reply runs the join immediately, the `Enrol`
        // (`IKpsk1`) path a pasted key already takes: pinned and
        // trusted on both sides the moment the handshake completes.
        // No knock, no six-digit compare, and no `tcr peer pair`
        // afterwards, because `internet_admission` never answers
        // either from off the LAN and this reply's whole point is
        // that the friend may be off it.
        let joined = tcr::peer::pair::join(&store, &token, "this-mac").await?;
        println!(
            "peer invite: ok addr={} file={}",
            joined.addr,
            path.display()
        );
        return Ok(());
    }
    let label = a.label.clone().unwrap_or_else(|| "joining-mac".to_string());
    // This process runs no keeper of its own, so a live mapping is not
    // sitting in `reach::external_socket`; it is read off the state
    // file a serving process wrote, the same reader `peer moved mint`
    // uses (`peer::reach::recorded_external_socket`).
    let now_ms = tcr::peer::state::now_ms();
    let (external, reason) =
        tcr::peer::reach::recorded_external_socket(&peer_state_path(&path), now_ms);
    if let Some(reason) = reason {
        println!(
            "peer invite: {reason}, so this key carries what this Mac answers on here \
             alone"
        );
    }
    let (invite, token) = tcr::peer::pair::mint_invite(&store, &label, a.ttl, a.uses, external)?;
    println!(
        "peer invite: ok id={} label={} ttl_s={} uses={}",
        invite.id, invite.label, a.ttl, a.uses
    );
    if a.plain {
        println!("{}", token.to_token());
    } else {
        println!("{}", token.to_token_v3());
        println!(
            "peer invite: a Mac on 1.1.9 or older cannot read this key and will say it \
             is not a join key; tcr peer invite --plain mints the readable form it can \
             read"
        );
    }
    // One line per address the key carries, in the order the joiner
    // will try them, so the operator sending this key can see which
    // paths their friend actually has. The key itself is unchanged by
    // what is printed here.
    for entry in &token.addrs {
        println!("peer invite: {} {}", entry.kind.label(), entry.addr);
    }
    if !token
        .addrs
        .iter()
        .any(|entry| entry.kind == tcr::peer::config::join_key::DialAddressKind::Internet)
    {
        println!(
            "peer invite: this key carries no internet address, so a friend who is not \
             on this network or this tailnet needs this Mac's router to forward the \
             port; `tcr peer reach` reports where that stands"
        );
    }
    println!(
        "peer invite: this key is join-capable by anything that can read {} until it \
         is used or expires, `tcr peer invite --revoke {}` ends it early",
        path.display(),
        invite.id
    );
    Ok(())
}

/// `tcr peer join`: join another Mac using a key it printed.
pub(super) async fn run_peer_join(a: peer_cli::PeerJoinArgs) -> anyhow::Result<()> {
    let path = a.peers.path();
    let store = tcr::peer::config::PeerStore::open(&path)?;
    // Parsed once into one typed value, so no later line here has to
    // ask "link or key?" with a different test. Nothing below prints
    // any field of it: both halves are live secrets and a terminal
    // keeps scrollback.
    let source = if a.stdin {
        tcr::peer::pair::TokenSource::Stdin
    } else {
        let Some(key) = a.key.as_deref() else {
            anyhow::bail!(
                "peer join: give the key or link as an argument, or pass --stdin and \
                 pipe it in (which keeps it out of `ps` and the shell history)"
            );
        };
        tcr::peer::pair::TokenSource::Argv(key)
    };
    let input = tcr::peer::pair::read_join_input(source)?;

    if let tcr::peer::pair::JoinInput::Ask(ask) = &input {
        let now_ms = tcr::peer::state::now_ms();
        let (external, reason) =
            tcr::peer::reach::recorded_external_socket(&peer_state_path(&path), now_ms);
        if let Some(reason) = reason {
            println!(
                "peer join: {reason}, so this reply carries what this Mac answers on \
                 here alone"
            );
        }
        // The friend answering an ask mints a one-use, ten-minute join
        // key for itself, the same `mint_invite` `tcr peer invite`
        // runs, and seals THAT to the ask rather than a bare address:
        // off the LAN, `internet_admission` (`src/peer/listener.rs`)
        // answers only `Enrol` (a minted key), never a knock, so a
        // reply that only taught an address could never be dialled by
        // the friend this mode exists for.
        let (_invite, token) =
            tcr::peer::pair::mint_invite(&store, "sealed-invite-reply", 600, 1, external)?;
        let reply = tcr::peer::ask::seal_key(ask, &token)?;
        println!("{}", reply.to_string_wire());
        println!(
            "peer join: send this back to whoever sent you the invite; nothing in it \
             says where you are except to them"
        );
        println!(
            "peer join: opening it joins you immediately, pinned and trusted on both \
             sides, the same as a pasted key"
        );
        return Ok(());
    }

    // The network key first, because it is what a link with no join
    // key is FOR, and because setting it is what lets this Mac see the
    // office at all.
    //
    // A refusal here used to `bail!` the whole command, which meant a
    // link carrying both a network key and a join key paired nothing
    // at all when the network key alone was refused: the join token
    // was never even read. The refusal is now reported and the
    // command carries on to the join token below, so a link with
    // both keys still pairs on a Mac that keeps its own network key.
    let mut network_key_refusal: Option<String> = None;
    if let Some(network_key) = input.network_key() {
        let _lock = tcr::peer::config::FileLock::acquire(&path)?;
        let mut file = tcr::peer::config::read_or_default(&path)?;
        // The same refusal `tcr peer network-key join` gives, in the
        // same words. This arm used to overwrite in silence, which made
        // one clicked link enough to cut a Mac off from the mesh it was
        // already on, and the documented promise that the key is not
        // replaced without asking was true of one command out of two.
        match file.set_network_key(network_key, a.replace) {
            Err(pinned) => {
                network_key_refusal = Some(format!(
                    "peer join: a network key is already set on this Mac, and this link \
                     carries another. Taking it cuts this Mac off from every Mac still \
                     holding the old one, {pinned} pinned peer(s) here. Pass --replace to mean \
                     it, or `tcr peer network-key clear` first"
                ));
            }
            Ok(replaced) => {
                tcr::peer::config::save(&path, &file)?;
                println!(
                    "peer join: network key {} file={}",
                    if replaced { "replaced" } else { "set" },
                    path.display()
                );
            }
        }
    }

    let Some(token) = input.join_token() else {
        // No join token to fall back on: the network-key refusal, if
        // any, is the whole outcome, so it is the error.
        if let Some(refusal) = network_key_refusal {
            anyhow::bail!(refusal);
        }
        println!(
            "peer join: that link carries no join key, so nothing was paired, it \
             sets the network key and stops there. Pair from either Mac with \
             `tcr peer pair <host:port>`"
        );
        return Ok(());
    };
    // A join token exists, so this link still pairs even though its
    // network key was refused above: say so, in the same line shape
    // as the "ok" line below, rather than leaving the earlier
    // refusal looking like the whole outcome.
    if let Some(refusal) = &network_key_refusal {
        println!("{refusal} (this link also carries a join key, which was still used below)");
    }
    let label = a.label.clone().unwrap_or_else(|| "this-mac".to_string());
    // The address that ANSWERED, not the first one in the key: a key
    // carries every address that Mac can be reached at, and the one
    // that worked is the only one worth printing back.
    let joined = tcr::peer::pair::join(&store, token, &label).await?;
    println!("peer join: ok addr={} file={}", joined.addr, path.display());
    Ok(())
}

/// `tcr peer forget`: stop trusting a Mac.
pub(super) fn run_peer_forget(a: peer_cli::PeerForgetArgs) -> anyhow::Result<()> {
    let path = a.peers.path();
    let store = tcr::peer::config::PeerStore::open(&path)?;
    let peer = parse_peer_arg("peer forget", &a.peer)?;
    if !tcr::peer::pair::forget(&store, &peer)? {
        println!("peer forget: not-found peer={}", a.peer);
        return Ok(());
    }
    println!("peer forget: ok peer={}", a.peer);
    println!(
        "peer forget: its next handshake fails the pin check before message 2, and any \
         live session with it closes within one frame"
    );
    let file = tcr::peer::config::read_or_default(&path)?;
    let relays = tcr::peer::pair::peers_still_holding_relay(&file);
    if !relays.is_empty() {
        println!(
            "peer forget: WARNING this does NOT revoke egress still reachable through \
             {} peer(s) holding `forward`",
            relays.len()
        );
        println!(
            "peer forget: revoke that grant on the middle peer, or set maxHops 0, which \
             makes revocation mesh-wide again"
        );
    }
    println!(
        "peer forget: WARNING if a dead drop was ever on for this peer, it still holds \
         the pair's rendezvous secret and can read and write the drop until a re-pair \
         mints a new one"
    );
    Ok(())
}

/// `tcr peer hello`: say hello to one pinned peer over the address already on its row.
pub(super) async fn run_peer_hello(a: peer_cli::PeerHelloArgs) -> anyhow::Result<()> {
    let path = a.peers.unwrap_or_else(tcr::peer::config::default_path);
    let store = tcr::peer::config::PeerStore::open(&path)?;
    let peer = parse_peer_arg("peer hello", &a.peer)?;
    match tcr::peer::serve::say_hello(&store, &peer).await? {
        Some(_theirs) => println!("peer hello: ok peer={}", a.peer),
        None => println!(
            "peer hello: no-answer peer={} (not pinned, or nothing on its row answered)",
            a.peer
        ),
    }
    Ok(())
}

/// What `tcr peer pair` dials, from what an operator typed.
///
/// A `host:port` is taken as it is. A BARE address gets the port this Mac's
/// own default listener uses, because that is the only port a knock with no
/// port in it could have come from, and it is what every answer dialled before
/// a knock carried a port at all. `tcr peer pending` prints a bare address on
/// exactly that row, and a command that refused what the row beside it prints
/// sends the operator to look up a number the file already knows.
fn peer_dial_addr(text: &str) -> anyhow::Result<std::net::SocketAddr> {
    let text = text.trim();
    if let Ok(addr) = text.parse::<std::net::SocketAddr>() {
        return Ok(addr);
    }
    if let Ok(host) = text.parse::<std::net::IpAddr>() {
        return Ok(std::net::SocketAddr::new(
            host,
            tcr::peer::config::default_listen().port(),
        ));
    }
    anyhow::bail!("peer pair: {text} is not a host:port, or a bare host, to dial")
}

#[cfg(test)]
mod peer_dial_addr_tests {
    use super::peer_dial_addr;

    #[test]
    fn a_host_and_port_is_dialled_exactly_as_typed() {
        let addr = peer_dial_addr("192.0.2.10:7766").expect("a host:port parses");
        assert_eq!(addr.port(), 7766);
        assert_eq!(addr.ip().to_string(), "192.0.2.10");
    }

    #[test]
    fn a_bare_host_takes_the_default_listen_port() {
        let addr = peer_dial_addr("192.0.2.10").expect("a bare host parses");
        assert_eq!(
            addr.port(),
            tcr::peer::config::default_listen().port(),
            "a knock that named no port is answered where the default listener is"
        );
    }

    #[test]
    fn a_bracketed_ipv6_host_and_port_is_dialled_as_typed() {
        let addr = peer_dial_addr("[2001:db8::1]:7766").expect("a bracketed v6 host:port parses");
        assert_eq!(addr.port(), 7766);
    }

    #[test]
    fn a_bare_ipv6_host_takes_the_default_listen_port() {
        let addr = peer_dial_addr("2001:db8::1").expect("a bare v6 host parses");
        assert_eq!(addr.port(), tcr::peer::config::default_listen().port());
    }

    #[test]
    fn a_name_that_is_no_address_at_all_is_refused_rather_than_guessed() {
        let err = peer_dial_addr("not-an-address").expect_err("a hostname is not dialled");
        assert!(
            format!("{err:#}").contains("is not a host:port"),
            "the refusal names what was wrong with it: {err:#}"
        );
    }
}
