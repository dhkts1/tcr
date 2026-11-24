//! `tcr peer`, the LAN peer mesh: the dispatcher and the helpers several verbs share. Each
//! verb group's body lives in a child module named for it.

use std::path::{Path, PathBuf};

use tcr::peer;

mod dead_drop;
mod exits;
mod find;
mod grants;
mod graph;
mod knock;
pub(crate) mod ls;
mod moved;
mod network_key;
mod pairing;
mod reach;
mod tailnet;

/// `tcr peer …`, the argument shape for the LAN peer mesh.
///
/// The two verbs on the common path are `find` and `share`, one switch each and
/// both off by default: finding opens a port, announces presence (and a name,
/// if allowed) and carries bytes it cannot read, while sharing lets another
/// machine read the requests it serves. A switch that silently included the
/// other would hide the one disclosure that matters. Everything else here is a
/// setting, present for the operator who wants it and out of the way of the one
/// who does not.
///
/// Like `tcr group`, this shape is a CONTRACT with the TcrBar panel, which
/// shells out to it. The panel never speaks HTTP to the proxy and never reads
/// the config file, so every write it makes is an argv built from this surface
/// and every read is a `--json` sibling document.
///
/// `#[expect(dead_code)]`: the fields below are the surface, and the phase that
/// fills `run_peer`'s bodies is what starts reading them. It deletes this line
/// on the way past, the suppression is deliberately on the module rather than
/// on each struct inside it, so there is one thing to delete and one place to
/// look.
#[expect(
    dead_code,
    reason = "the CLI surface ahead of the phase that reads it; see above"
)]
pub(crate) mod peer_cli;

/// `tcr peer id|ls|find|name|share|pair|invite|join|forget|allow|via`, the
/// LAN peer mesh.
///
/// A thin dispatcher, the same shape as [`run_group`]: every body belongs to the
/// phase that builds that verb, and the phase each one waits on is named in its
/// `todo!`. Reading this function tells you what the mesh will do; reading
/// `src/peer/` tells you what it has to hold while doing it.
pub(crate) async fn run_peer(args: peer_cli::PeerArgs) -> anyhow::Result<()> {
    use peer_cli::PeerAction;

    match args.action {
        PeerAction::Id(a) => ls::run_peer_id(a),
        PeerAction::Ls(a) => ls::run_peer_ls(a),
        PeerAction::Find(a) => find::run_peer_find(a).await,
        PeerAction::Name(a) => pairing::run_peer_name(a),
        PeerAction::Share(a) => grants::run_peer_share(a),
        PeerAction::Pair(a) => pairing::run_peer_pair(a).await,
        PeerAction::Invite(a) => pairing::run_peer_invite(a).await,
        PeerAction::Join(a) => pairing::run_peer_join(a).await,
        PeerAction::Forget(a) => pairing::run_peer_forget(a),
        PeerAction::Hello(a) => pairing::run_peer_hello(a).await,
        PeerAction::Allow(a) => grants::run_peer_allow(a),
        PeerAction::Lend(a) => grants::run_peer_lend(a),
        PeerAction::Via(a) => exits::run_peer_via(a),
        PeerAction::Pending(a) => knock::run_peer_pending(a),
        PeerAction::Accept(a) => knock::run_peer_accept(a),
        PeerAction::Ignore(a) => knock::run_peer_ignore(a),
        PeerAction::Block(a) => knock::run_peer_block(a),
        PeerAction::Unblock(a) => knock::run_peer_unblock(a),
        PeerAction::NetworkKey(a) => network_key::run_peer_network_key(a),
        PeerAction::Link(a) => network_key::run_peer_link(a),
        PeerAction::Moved(a) => moved::run_peer_moved(a),
        PeerAction::Reach(a) => reach::run_peer_reach(a),
        PeerAction::Tailnet(a) => tailnet::run_peer_tailnet(a).await,
        PeerAction::Internet(a) => reach::run_peer_internet(a),
        PeerAction::Drop(a) => dead_drop::run_peer_drop(a),
        PeerAction::DropStore(a) => dead_drop::run_peer_drop_store(a),
        PeerAction::Account(a) => exits::run_peer_account(a),
        PeerAction::Status(a) => graph::run_peer_status(a).await,
        PeerAction::Graph(a) => graph::run_peer_graph(a).await,
    }
}

/// Seconds since the unix epoch, for the port slot.
fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or_default(|since| since.as_secs())
}

/// The runtime-state file that goes with a peers file.
///
/// Derived from the peers path rather than taken as a second argument, for the
/// reason `PeerIdArgs::peers` already gives for the node-key directory: one
/// `--peers` points the WHOLE peer surface at a temp dir, and a test that had
/// to remember a second flag would be a test that forgets it and reads the
/// operator's real cache directory.
fn peer_state_path(peers_path: &Path) -> PathBuf {
    // The DEFAULT peers file lives in the config directory and the state file
    // belongs in the cache directory, a split `state::default_path` already owns,
    // so the default path is never derived here, and only an explicit `--peers`
    // somewhere else moves the state file with it.
    if peers_path == peer::config::default_path() {
        return peer::state::default_path();
    }
    match peers_path.parent() {
        Some(parent) => parent.join("peer-state.json"),
        None => peer::state::default_path(),
    }
}

/// The peers file at `peers_path`, read under its lock.
///
/// The lock comes back with the file and must stay bound for as long as the
/// caller may write the file back: a peers-file mutation is a read-modify-write,
/// and only the held lock keeps a listener's enrolment from landing in between.
fn lock_peers_file(
    peers_path: &Path,
) -> anyhow::Result<(peer::config::FileLock, peer::config::PeerFile)> {
    let lock = peer::config::FileLock::acquire(peers_path)?;
    let file = peer::config::read_or_default(peers_path)?;
    Ok((lock, file))
}

/// The runtime-state file that goes with `peers_path`, read under its lock at
/// the current clock: its path, that clock, the held lock and the state.
fn lock_peer_state(
    peers_path: &Path,
) -> anyhow::Result<(PathBuf, i64, peer::config::FileLock, peer::state::PeerState)> {
    let state_path = peer_state_path(peers_path);
    let now = peer::pair::now_ms();
    let lock = peer::config::FileLock::acquire(&state_path)?;
    let state = peer::state::load(&state_path, now)?;
    Ok((state_path, now, lock, state))
}

/// A `<peer>` argument parsed as a peer id, its refusal prefixed with the verb.
fn parse_peer_arg(verb: &str, raw: &str) -> anyhow::Result<tcr_peer_wire::PeerId> {
    tcr_peer_wire::PeerId::parse(raw).map_err(|refusal| anyhow::anyhow!("{verb}: {refusal}"))
}

/// The wire's own name for a window, so a printed line and a JSON field cannot
/// disagree about what `7d` is called.
const fn peer_window_name(window: tcr_peer_wire::Window) -> &'static str {
    match window {
        tcr_peer_wire::Window::FiveHour => "5h",
        tcr_peer_wire::Window::SevenDay => "7d",
        tcr_peer_wire::Window::SevenDayOi => "7d_oi",
        tcr_peer_wire::Window::Unknown => "unknown",
    }
}

/// The one sanitizer every peer label passes through, applied a SECOND time
/// on the way out of `tcr peer ls --json` and `tcr peer id --regenerate`'s
/// eviction list.
///
/// Every entry point that ACCEPTS a label already refuses one that fails
/// [`tcr_peer_wire::sanitize_label`] (an `@`, a UUID shape, an organization
/// name, over-length), so a row read back from a file this program wrote
/// should always already be clean. This repository is public, though, and the
/// peers file is hand-editable JSON: a corrupt or hand-planted row must not
/// reach a fixture or a screen unmasked just because it skipped the accepting
/// check. A label that fails re-sanitizing is replaced outright rather than
/// partially redacted, because a partial redaction ("al***@example.com") is
/// itself a disclosure of the shape it is trying to hide.
fn masked_label(label: &str) -> String {
    tcr_peer_wire::sanitize_label(label).unwrap_or_else(|_| "[masked]".to_string())
}

/// One pending row as a READER of it wants it: `addr` carrying the port the
/// knocker said to answer on, when it said one, and the proposed name masked.
///
/// The row in the file keeps the bare IP, which is the key the mutes, the bans
/// and the accepted windows are all matched on
/// (`peer::state::Knock::addr`). What comes out of `tcr peer ls --json` and
/// `tcr peer pending` is the address to DIAL, because that string is handed
/// straight to `tcr peer pair`, by an operator reading a terminal and by the
/// panel's own Accept. Printing the key there sent every answer to the default
/// port.
///
/// **The name is masked here and not at one call site.** A proposed name is
/// text a stranger's Mac chose, already sanitized on arrival, and masked again
/// on the way out for the reason [`masked_label`] gives: this repository is
/// public and the state file is hand-editable JSON. `ls --json` masked it and
/// `pending` did not, which is one rule with two answers, and the surface that
/// skipped it is the one a menu bar and a notification banner now read.
///
/// One function for both surfaces rather than the same two maps twice: the
/// panel and the terminal have to be told the same address and the same name,
/// and the copy that drifts is the one nobody runs.
fn pending_row_for_readers(mut knock: peer::state::Knock) -> peer::state::Knock {
    knock.addr = knock.dial_address();
    knock.proposed_name = knock.proposed_name.as_deref().map(masked_label);
    knock
}

/// Give this Mac a peer port when the operator turns one of the features that need one
/// on and there is none yet. Returns the address written, or `None` when
/// `listen` was already set, which this never touches.
///
/// The caller writes the file and then prints [`print_listen_written`]: the
/// verbs that call this already hold the peers-file lock and already have
/// a save of their own, and a second write here would be a second writer for
/// one key.
///
/// It is called on the ON arm only. Turning something OFF, or removing a grant,
/// is not an opt-in and must not open a port.
fn ensure_listen_for_opt_in(file: &mut peer::config::PeerFile) -> Option<std::net::SocketAddr> {
    if file.listen.is_some() {
        return None;
    }
    let chosen = peer::config::default_listen();
    file.listen = Some(chosen);
    Some(chosen)
}

/// What every verb that just wrote a peer port says, in one place so they
/// all say the same thing.
///
/// The second line is the honest half. The peer socket is bound once, at boot,
/// from the file as it was then (`src/server.rs`, the `file.listen` read before
/// `listener::bind`); `PeerStore::reload_if_changed` re-reads the policy half
/// of this file whenever its mtime moves but nothing re-binds a socket, so a
/// port written now is a port that opens at the next start and not before.
fn print_listen_written(addr: std::net::SocketAddr, peers_path: &Path) {
    for line in listen_written_lines(addr, peers_path) {
        println!("{line}");
    }
}

/// [`print_listen_written`]'s two lines, for a verb whose standard output is
/// a JSON document and which says them on standard error instead.
fn listen_written_lines(addr: std::net::SocketAddr, peers_path: &Path) -> [String; 2] {
    [
        format!(
            "peer.listen: {addr} written into {} (this Mac had no peer port, and turning this \
             on needs one)",
            peers_path.display()
        ),
        "peer.listen: nothing is listening on it yet. The port opens the next time the proxy \
         starts, so quit TcrBar and open it again to finish turning this on"
            .to_string(),
    ]
}

#[cfg(test)]
mod peer_ls_masking_tests {
    use super::masked_label;

    #[test]
    fn an_email_a_uuid_and_an_org_name_come_out_masked() {
        assert_eq!(masked_label("alice@example.com"), "[masked]");
        assert_eq!(
            masked_label("11111111-1111-1111-1111-111111111111"),
            "[masked]"
        );
        // "Acme Corp!" carries a character the sanitizer's whitelist refuses,
        // the same defence that would refuse a known organization name,
        // since this crate holds no live org list to match against.
        assert_eq!(masked_label("Acme Corp!"), "[masked]");

        // A plain label is untouched, so the mask is not swallowing everything,
        // the failure mode a lone masking test cannot see.
        assert_eq!(masked_label("studio-mac"), "studio-mac");
    }
}
