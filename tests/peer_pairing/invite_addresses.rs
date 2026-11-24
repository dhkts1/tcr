// ---------------------------------------------------------------------------
// The key carries addresses a friend can dial
// ---------------------------------------------------------------------------

use super::harness::*;
use super::*;

/// **A mint on a wide listener never puts the bind address in the key.**
///
/// This is the bug in one line: a Mac listening on `0.0.0.0:7755` minted
/// `…:0.0.0.0:7755:…`, and the friend's `tcr peer join` dialled its own
/// machine and reported that nothing answered.
///
/// Watched red: with `dial_addresses` returning the listen socket whatever it
/// is, this fails with `left: [DialAddress { addr: 0.0.0.0:7755, kind: Chosen
/// }]`.
///
/// Both outcomes are asserted, because the answer depends on the machine
/// running the test: a host with no address of its own has nothing to put in a
/// key, and that has to be a refusal naming what to do rather than a key
/// nobody can use.
#[test]
fn a_mint_on_a_wide_listener_carries_no_bind_address() {
    let node = Node::new("wide-listener");
    let mut file = node.file();
    file.listen = Some("0.0.0.0:7755".parse().expect("a wide bind address"));
    node.write_file(&file);
    let store = config::PeerStore::open(&node.peers).expect("open the store");

    match pair::mint_invite_as(&store, &node.key, "laptop-2", 600, 1, None) {
        Ok((_invite, token)) => {
            assert!(
                !token
                    .sockets()
                    .any(|addr| addr.ip().is_unspecified() || addr.port() == 0),
                "the bind address is not a dial address: {:?}",
                token.sockets().collect::<Vec<_>>()
            );
            assert!(
                token.to_token().starts_with(pair::TOKEN_PREFIX_V2),
                "a key minted today carries a list"
            );
        }
        Err(refusal) => {
            let text = format!("{refusal:#}");
            assert!(
                text.contains("no address a friend could dial"),
                "a host with nothing dialable refuses and says what to do: {text}"
            );
        }
    }
}

/// **A pinned `listen` is an answered question**: the key carries that address
/// and nothing else, whatever else this Mac holds.
///
/// Watched red: drop the `is_unspecified` short circuit in `dial_addresses`
/// and this fails on the address list.
#[test]
fn a_mint_on_a_pinned_listener_carries_exactly_that_address() {
    let node = Node::new("pinned-listener");
    let mut file = node.file();
    file.listen = Some("127.0.0.1:9600".parse().expect("a loopback address"));
    node.write_file(&file);
    let store = config::PeerStore::open(&node.peers).expect("open the store");

    let (_invite, token) = pair::mint_invite_as(&store, &node.key, "laptop-2", 600, 1, None)
        .expect("an invite is minted");
    assert_eq!(
        token.sockets().collect::<Vec<_>>(),
        vec![
            "127.0.0.1:9600"
                .parse::<SocketAddr>()
                .expect("a loopback address")
        ],
        "somebody typed that address into the peers file, so it is the one they meant"
    );
}

/// **A v3 key round-trips every address family**, and the rendered string
/// hides them: no run of digits and dots, no bracketed run of hex and colons,
/// that a glance could read as an address.
///
/// Watched red: change `JoinToken::to_token_v3` to fall back to plain-text
/// addresses and this fails on the "no address is visible" assertion, or
/// break `dialaddrs::decode` and this fails on the round trip itself.
#[test]
fn a_v3_key_round_trips_every_address_family() {
    let token = pair::JoinToken::new(
        vec![
            "192.0.2.10:7755".parse().expect("a test address"),
            "[2001:db8::1]:7755".parse().expect("a test address"),
            "198.51.100.20:7755".parse().expect("a test address"),
        ],
        PeerId([5_u8; 32]),
        [9_u8; 32],
    );
    let rendered = token.to_token_v3();
    assert!(
        rendered.starts_with(pair::TOKEN_PREFIX_V3),
        "a v3 key is minted with its own prefix: {rendered}"
    );
    assert_eq!(
        pair::JoinToken::parse(&rendered).expect("a v3 key parses"),
        token,
        "every address family must survive the round trip"
    );
    assert_eq!(
        pair::JoinToken::parse(&format!("  {rendered}\n")).expect("a pasted v3 key parses"),
        token,
        "a paste carries whitespace"
    );

    let body = rendered
        .strip_prefix(pair::TOKEN_PREFIX_V3)
        .expect("the prefix was just asserted");
    assert!(
        !body.contains('.') && !body.contains(':'),
        "an opaque key must show nobody an address at a glance: {rendered}"
    );
}

/// **`tcr peer invite` says what each address in the key is**, on the surface
/// an operator reads before they paste it into a chat window.
///
/// The key itself is unchanged by these lines: they are what lets the person
/// sending it see that their friend on another network has nothing to dial
/// yet.
///
/// Watched red: delete the per-address loop from the `Invite` arm of
/// `run_peer` and this fails on the `chosen` line.
#[test]
fn tcr_peer_invite_names_every_address_and_the_internet_gap() {
    let node = Node::new("invite-cli");
    let mut file = node.file();
    file.listen = Some("127.0.0.1:9600".parse().expect("a loopback address"));
    node.write_file(&file);

    // `HOME` points at this test's own scratch directory, because `tcr peer
    // invite` loads this node's keypair from the config directory and a test
    // may not read or write the operator's real one.
    let home = scratch("invite-cli-home");
    let output = tcr_command(&home)
        .args(["peer", "invite", "--label", "laptop-2"])
        .args(["--peers", node.peers.to_str().expect("a utf-8 path")])
        .output()
        .expect("spawn tcr peer invite");
    let out = String::from_utf8_lossy(&output.stdout).to_string();
    let err = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(output.status.success(), "the mint failed: {out}{err}");

    assert!(
        out.contains(&format!("\n{}", pair::TOKEN_PREFIX_V3)),
        "the key printed by default is opaque now: {out}"
    );
    assert!(
        !out.contains(pair::TOKEN_PREFIX_V2),
        "a v2 key is not printed unless --plain asked for it: {out}"
    );
    assert!(
        out.contains("cannot read this key"),
        "the older-build sentence is the inviting side's job, since no new build can edit a \
         1.1.9 refusal: {out}"
    );
    assert!(
        out.contains("peer invite: chosen 127.0.0.1:9600"),
        "each address is named with what kind of path it is: {out}"
    );
    assert!(
        out.contains("carries no internet address"),
        "a key with nothing an outside friend can open says so, in one sentence: {out}"
    );
}

/// **The sealed exchange end to end, at the command line**: `invite --sealed`
/// mints an ask that carries no address, `join --stdin` answers it by
/// minting its own one-use join key and sealing that to the ask, and
/// `invite --reply --stdin` opens the reply and runs the join with the key
/// it carried.
///
/// This is the wiring test, not the live-join test: `tests/peer_ask.rs`
/// covers the five refusal shapes against the library directly, and the
/// container scenario (`tests/containers/scenarios/sealed-invite.sh`) is
/// where both the "no address in the chat" claim over a real grep AND the
/// join actually completing against a real listening server are measured.
/// This test proves the three CLI branches (`--sealed`, `--stdin` on an ask,
/// `--reply --stdin`) are wired to each other and to the real `Enrol` join
/// path, up to the point a live listener would be needed.
#[test]
fn the_sealed_exchange_works_end_to_end_at_the_command_line() {
    let inviter = Node::new("sealed-inviter");
    let mut inviter_file = inviter.file();
    inviter_file.listen = Some("127.0.0.1:9700".parse().expect("a loopback address"));
    inviter.write_file(&inviter_file);
    let inviter_home = scratch("sealed-inviter-home");

    let friend = Node::new("sealed-friend");
    let mut friend_file = friend.file();
    friend_file.listen = Some("127.0.0.1:9701".parse().expect("a loopback address"));
    friend.write_file(&friend_file);
    let friend_home = scratch("sealed-friend-home");

    let mint = tcr_command(&inviter_home)
        .args(["peer", "invite", "--sealed"])
        .args(["--peers", inviter.peers.to_str().expect("a utf-8 path")])
        .output()
        .expect("spawn tcr peer invite --sealed");
    let mint_out = String::from_utf8_lossy(&mint.stdout).to_string();
    assert!(
        mint.status.success(),
        "minting an ask failed: {mint_out}{}",
        String::from_utf8_lossy(&mint.stderr)
    );
    let ask_line = mint_out
        .lines()
        .find(|line| line.starts_with(tcr::peer::ask::ASK_PREFIX))
        .unwrap_or_else(|| panic!("no ask line on stdout: {mint_out}"))
        .to_string();
    assert!(
        !ask_line.contains("9700") && !ask_line.contains("127.0.0.1"),
        "an ask names no address at a glance: {ask_line}"
    );

    let mut answer = tcr_command(&friend_home)
        .args(["peer", "join", "--stdin"])
        .args(["--peers", friend.peers.to_str().expect("a utf-8 path")])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn tcr peer join --stdin");
    use std::io::Write as _;
    answer
        .stdin
        .as_mut()
        .expect("the child's stdin")
        .write_all(format!("{ask_line}\n").as_bytes())
        .expect("write the ask to stdin");
    let answer_out = answer.wait_with_output().expect("the child exits");
    let answer_stdout = String::from_utf8_lossy(&answer_out.stdout).to_string();
    assert!(
        answer_out.status.success(),
        "answering the ask failed: {answer_stdout}{}",
        String::from_utf8_lossy(&answer_out.stderr)
    );
    let reply_line = answer_stdout
        .lines()
        .find(|line| line.starts_with(tcr::peer::ask::REPLY_PREFIX))
        .unwrap_or_else(|| panic!("no reply line on stdout: {answer_stdout}"))
        .to_string();
    assert!(
        !reply_line.contains("127.0.0.1"),
        "a reply names no address at a glance either: {reply_line}"
    );

    let mut open = tcr_command(&inviter_home)
        .args(["peer", "invite", "--reply", "--stdin"])
        .args(["--peers", inviter.peers.to_str().expect("a utf-8 path")])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn tcr peer invite --reply --stdin");
    open.stdin
        .as_mut()
        .expect("the child's stdin")
        .write_all(format!("{reply_line}\n").as_bytes())
        .expect("write the reply to stdin");
    let open_out = open.wait_with_output().expect("the child exits");
    let open_out_text = format!(
        "{}{}",
        String::from_utf8_lossy(&open_out.stdout),
        String::from_utf8_lossy(&open_out.stderr)
    );
    // `friend` never ran a real server, only the one-shot `join --stdin` that
    // minted this key, so nothing is listening at 127.0.0.1:9701: the dial
    // this `--reply --stdin` runs refuses to connect. That failure is the
    // positive control: it proves opening the reply reached the `Enrol` join
    // attempt with the friend's REAL address and REAL secret, not that mode
    // B's whole flow completes without a listener. The live listener that
    // proves the join itself succeeds is
    // `tests/containers/scenarios/sealed-invite.sh`, against the real
    // shipped binary running as a real server.
    assert!(
        open_out_text.contains("127.0.0.1:9701"),
        "opening the reply attempted a join with the friend's real address: {open_out_text}"
    );
    assert!(
        !open_out_text.contains("tcr peer pair") && !open_out_text.contains("they answer at"),
        "no dial-later or bare-address line: the reply carried a key, and the failure is the \
         join's own, not a leftover from the old bare-address ending: {open_out_text}"
    );
}

/// **`tcr peer invite --plain` mints exactly the readable v2 key**, and none
/// of the v3-only sentences that only make sense next to an opaque one.
#[test]
fn tcr_peer_invite_plain_mints_the_readable_key() {
    let node = Node::new("invite-cli-plain");
    let mut file = node.file();
    file.listen = Some("127.0.0.1:9600".parse().expect("a loopback address"));
    node.write_file(&file);

    let home = scratch("invite-cli-plain-home");
    let output = tcr_command(&home)
        .args(["peer", "invite", "--label", "laptop-2", "--plain"])
        .args(["--peers", node.peers.to_str().expect("a utf-8 path")])
        .output()
        .expect("spawn tcr peer invite --plain");
    let out = String::from_utf8_lossy(&output.stdout).to_string();
    let err = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(output.status.success(), "the mint failed: {out}{err}");

    assert!(
        out.contains(&format!("\n{}", pair::TOKEN_PREFIX_V2)),
        "--plain still mints the readable key: {out}"
    );
    assert!(
        !out.contains(pair::TOKEN_PREFIX_V3),
        "--plain does not also mint the opaque one: {out}"
    );
    assert!(
        !out.contains("cannot read this key"),
        "the older-build sentence is about the opaque key and does not belong here: {out}"
    );
}
