//! The Macs knocking to pair: `tcr peer pending`, `accept`, `ignore`, `block` and `unblock`.

use tcr::peer;

use super::peer_cli;
use super::{lock_peer_state, peer_state_path, pending_row_for_readers};

/// `tcr peer pending`: show the Macs asking to connect with this one.
pub(super) fn run_peer_pending(a: peer_cli::PeerPendingArgs) -> anyhow::Result<()> {
    let peers_path = a.peers.unwrap_or_else(peer::config::default_path);
    let state_path = peer_state_path(&peers_path);
    let now = peer::state::now_ms();
    let state = peer::state::load(&state_path, now)?;
    // Every read surface in this verb reads `visible_pending`, for the
    // reason `PeerState::visible_pending` gives: a reservation
    // placeholder is a slot held against the cap, not a Mac the
    // operator saw ask.
    // `pending_row_for_readers` for the reason it gives: what a reader
    // of a pending row does with the address is dial it, so both
    // surfaces here print the port the knocker said to answer on, and
    // the name a stranger's Mac proposed is masked on the way out of
    // both, which this verb used to skip while `ls --json` did it.
    let pending: Vec<peer::state::Knock> = state
        .visible_pending()
        .into_iter()
        .map(pending_row_for_readers)
        .collect();

    if a.json {
        #[derive(serde::Serialize)]
        #[serde(rename_all = "camelCase")]
        struct PendingJson {
            pending: Vec<peer::state::Knock>,
            muted: Vec<peer::state::Mute>,
            banned: Vec<peer::state::Ban>,
        }
        println!(
            "{}",
            serde_json::to_string(&PendingJson {
                pending,
                muted: state.muted.clone(),
                banned: state.banned,
            })?
        );
        return Ok(());
    }

    if pending.is_empty() {
        println!("no Macs are asking to connect");
    }
    for knock in &pending {
        // `<name or address> wants to connect`, which is the row
        // `abuse-resistance.md` specifies, plus the instance id so the
        // operator can name it to `accept` unambiguously.
        println!(
            "{}: pending: {} ({}) wants to connect  instance={} wire={}",
            knock.addr,
            knock.proposed_name.as_deref().unwrap_or(&knock.addr),
            knock.addr,
            knock.instance_id,
            knock.wire_version
        );
    }
    for line in state.refusal_lines() {
        println!("{line}");
    }
    Ok(())
}

/// `tcr peer accept`: approve one pairing request.
pub(super) fn run_peer_accept(a: peer_cli::PeerDecideArgs) -> anyhow::Result<()> {
    let (state_path, now, _lock, mut state) = lock_peer_state(&a.peers.path())?;
    let Some(window) = state.accept_knock(&a.target, now, peer::pair::PAIRING_WINDOW_SECS) else {
        anyhow::bail!(
            "peer accept: no pairing request matches {:?} (`tcr peer pending` lists \
             them; a request expires on its own after ten minutes)",
            a.target
        );
    };
    peer::state::save(&state_path, &state)?;
    println!(
        "peer accept: ok instance={} addr={} until={} unix-ms",
        window.instance_id, window.addr, window.until_ms
    );
    println!(
        "peer accept: that ONE Mac may now start a first pairing for {} seconds; \
         both screens will show six digits and both of you press trust",
        peer::pair::PAIRING_WINDOW_SECS
    );
    Ok(())
}

/// `tcr peer ignore`: turn one pairing request down and stay quiet to that address.
pub(super) fn run_peer_ignore(a: peer_cli::PeerDecideArgs) -> anyhow::Result<()> {
    let (state_path, now, _lock, mut state) = lock_peer_state(&a.peers.path())?;
    // An address the operator wants quiet need not have a live
    // request: "make this stop" is a reasonable thing to type at a
    // row that just expired, and refusing it would send them to
    // `block` for something a mute answers.
    let addr = knock_target_addr("peer ignore", &state, &a.target)?;
    state.mute(&addr, now);
    peer::state::save(&state_path, &state)?;
    println!(
        "peer ignore: ok addr={addr} muted_for_s={}",
        peer::state::MUTE_MS / 1000
    );
    println!("peer ignore: knocks from it get nothing until the mute lifts on its own");
    Ok(())
}

/// `tcr peer block`: never hear from that Mac again.
pub(super) fn run_peer_block(a: peer_cli::PeerDecideArgs) -> anyhow::Result<()> {
    let (state_path, now, _lock, mut state) = lock_peer_state(&a.peers.path())?;
    // Same rule as `ignore`, and it matters more here: a ban recorded
    // against something that is not an address is a ban the listener's
    // comparison never equals, and the command said ok.
    let addr = knock_target_addr("peer block", &state, &a.target)?;
    // **Both halves, and this is the only moment the key half is
    // reachable.** A knock reveals no static key, so a request blocked
    // before it ever dialled can only be banned by address; one that
    // got as far as an `XX` handshake left its key on the accepted
    // window, which is kept after the window closes for exactly this
    // reason (`PeerState::expire`), and banning it is what stops a DHCP
    // move from undoing the block.
    let key = state.key_learned_at(&addr);
    state.ban(&addr, key, peer::state::BanReason::Blocked, now);
    peer::state::save(&state_path, &state)?;
    println!(
        "peer block: ok addr={addr} key={}",
        key.map_or_else(|| "not-learned".to_string(), |key| key.display())
    );
    if key.is_none() {
        println!(
            "peer block: no handshake with it ever revealed a static key, so this \
             blocks the ADDRESS only, a new address on the same Mac would get \
             through"
        );
    }
    println!("peer block: `tcr peer unblock {addr}` lifts it; nothing else does");
    Ok(())
}

/// `tcr peer unblock`: lift a block.
pub(super) fn run_peer_unblock(a: peer_cli::PeerUnblockArgs) -> anyhow::Result<()> {
    let (state_path, _now, _lock, mut state) = lock_peer_state(&a.peers.path())?;
    if !state.unblock(&a.addr) {
        println!("peer unblock: not-found addr={}", a.addr);
        return Ok(());
    }
    peer::state::save(&state_path, &state)?;
    println!("peer unblock: ok addr={}", a.addr);
    Ok(())
}

/// The address `tcr peer ignore|block <target>` acts on: the address of the
/// pending request `target` names, or `target` itself when it is a bare knock
/// address with no live request.
///
/// It does have to BE an address, though. Anything else used to be stored
/// verbatim, and the listener compares against the bare IP a connection arrives
/// on, so the mute or ban was one nothing could ever equal and the command
/// printed ok. See `PeerState::is_knock_address`.
fn knock_target_addr(
    verb: &str,
    state: &peer::state::PeerState,
    target: &str,
) -> anyhow::Result<String> {
    match state.find_pending(target) {
        Some(knock) => Ok(knock.addr.clone()),
        None if peer::state::PeerState::is_knock_address(target) => Ok(target.to_string()),
        None => anyhow::bail!(
            "{verb}: {target:?} matches no pairing request and is not an address either \
             (`tcr peer pending` lists the requests; an address is the bare IP a knock \
             arrived from, with no port)"
        ),
    }
}
