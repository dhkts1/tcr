//! The accept gate `peer.internet` turns on, the address the far side sees this
//! Mac at, and the home-LAN IPv6 case row 14 got wrong.

use super::*;

// ---------------------------------------------------------------------------
// Item 2: the accept gate `peer.internet` turns on
// ---------------------------------------------------------------------------
//
// # Why three of the four addresses are driven by value
//
// macOS puts only `127.0.0.1`, `::1` and one link-local address on `lo0`
// (`tests/peer_abuse.rs` measured it: binding `127.0.0.2` fails with
// `EADDRNOTAVAIL`), and adding an alias needs root on a machine that is also
// serving a live proxy. So `10.0.0.7` and `2001:db8::1` cannot be dialled from
// here at all. They are driven through the address parameter
// `listener::serve_accepted` already takes, which is the same value
// `serve_connection` reads inside the accept loop and not a copy of it, and
// the wire-bytes half of the invariant is kept honest by one real-socket leg
// from the one address this box has. That is the split
// `tests/peer_pairing.rs:872-884` uses for the same reason.

/// One Mac's files: a peers file, a state file beside it, a node key.
struct GateNode {
    dir: std::path::PathBuf,
    peers: std::path::PathBuf,
    state: std::path::PathBuf,
    key: NodeKey,
}

impl GateNode {
    /// A node whose `peer.internet` is `internet` and which pins nobody.
    fn new(tag: &str, internet: bool) -> Self {
        let dir = scratch(tag);
        let peers = dir.join("tcr-peers.json");
        let state = dir.join("peer-state.json");
        let file = PeerFile {
            internet,
            ..PeerFile::default()
        };
        peer_config::save(&peers, &file).expect("write the peers file");
        let key = NodeKey::load_or_mint(&dir).expect("mint a node key");
        Self {
            dir,
            peers,
            state,
            key,
        }
    }

    fn context(&self) -> SessionContext {
        SessionContext::new(&self.key, &self.peers, &self.state)
    }

    fn config_dir(&self) -> &Path {
        &self.dir
    }
}

/// An `NN` message 1, built exactly as `noise::send_knock` builds its own.
fn knock_message_1() -> Vec<u8> {
    let throwaway = noise::random_secret().expect("a throwaway secret");
    let mut state = noise::initiator_with_secret(&throwaway, noise::PATTERN_KNOCK, None, None)
        .expect("a knock initiator");
    let mut scratch = vec![0_u8; 1024];
    let len = state
        .write_message(&[], &mut scratch)
        .expect("knock message 1");
    scratch[..len].to_vec()
}

/// An `IK` message 1 from a Mac this node has never pinned, aimed at this
/// node's real static key.
///
/// A real handshake and not a 96-byte filler: what this gate has to show is
/// that an `IK` from off the LAN reaches the PIN CHECK, and a frame that could
/// not be decrypted would be refused a step earlier for a reason that has
/// nothing to do with the gate.
fn ik_message_1_at(responder: &NodeKey) -> Vec<u8> {
    let (secret, _public) = noise::generate_static().expect("a stranger's keypair");
    let remote = responder.id().0;
    let mut state =
        noise::initiator_with_secret(&secret, noise::PATTERN_RETURN, Some(&remote), None)
            .expect("an IK initiator");
    let mut scratch = vec![0_u8; 1024];
    let len = state
        .write_message(&[], &mut scratch)
        .expect("IK message 1");
    scratch[..len].to_vec()
}

/// Hand `message_1` to the shipped connection handler as if it had been
/// accepted from `from` on a listener bound to `bind`, and report `(what the
/// handler said, bytes written back)`.
///
/// An in-memory duplex rather than a socket, because `from` is the whole point
/// and this box has one address. Everything under the call is the shipped
/// code: `listener::serve_accepted` is `serve_connection`, which is what the
/// accept loop runs. `bind` is likewise named rather than really bound, the
/// same reason: this box cannot bind `0.0.0.0` in a test that also wants a
/// scratch port free of every sibling test binary's own listeners.
async fn offer_from(
    context: SessionContext,
    from: SocketAddr,
    bind: SocketAddr,
    message_1: &[u8],
) -> (Result<(), String>, usize) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let (theirs, ours) = tokio::io::duplex(8192);
    let served =
        tokio::spawn(async move { listener::serve_accepted(ours, &context, &from, bind).await });

    let mut theirs = theirs;
    let mut framed = Vec::with_capacity(2 + message_1.len());
    framed.extend_from_slice(
        &u16::try_from(message_1.len())
            .expect("a short frame")
            .to_be_bytes(),
    );
    framed.extend_from_slice(message_1);
    theirs.write_all(&framed).await.expect("write message 1");
    theirs.flush().await.expect("flush message 1");

    let mut back = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), theirs.read_to_end(&mut back))
        .await
        .log_debug("waiting for the handler's reply")
        .and_then(|read| read.log_debug("reading the handler's reply"));
    let outcome = match served.await.expect("the handler task") {
        Ok(()) => Ok(()),
        Err(err) => Err(format!("{err:#}")),
    };
    (outcome, back.len())
}

/// **The gate for item 2, the allow list itself.**
///
/// Every prefix decision row 14 names is asserted as LAN, and the addresses
/// this repository uses for documentation are asserted as not-LAN. The second
/// half is the one with teeth: a gate written as a negation would call
/// `2001:db8::1` LAN the day a new prefix is assigned, and this is what says
/// it does not.
#[test]
fn lan_scope_is_the_allow_list_row_14_names() {
    for lan in [
        "127.0.0.1",
        "127.9.9.9",
        "::1",
        "10.0.0.7",
        "172.16.0.1",
        "172.31.255.254",
        "192.168.1.4",
        "fe80::1",
        "fd00::1",
        "fc00::1",
        // One past the edge of Tailscale's /48, still plain unique-local.
        "fd7a:115c:a1e1::1",
        // A dual-stack listener reports a v4 peer as v4-mapped, and that is
        // the same LAN it would be reported as on a v4 socket.
        "::ffff:10.0.0.7",
    ] {
        let addr: IpAddr = lan.parse().expect("an address");
        assert!(
            listener::is_lan_scope(addr),
            "{lan} is on decision row 14's list and must read as LAN scope"
        );
    }

    for internet in [
        "203.0.113.7",
        "198.51.100.1",
        "192.0.2.1",
        "8.8.8.8",
        // Just outside each range whose edge is easy to get wrong.
        "172.15.255.255",
        "172.32.0.1",
        "100.63.255.255",
        "100.128.0.1",
        "169.253.0.1",
        // RFC 6598 carrier-grade NAT, which is what a tailnet is numbered out
        // of, and RFC 3927 link-local. Both used to read as LAN, which
        // admitted every tailnet node on earth to the knock path whatever the
        // internet switch said.
        "100.64.0.1",
        "100.127.255.254",
        "169.254.1.1",
        "169.254.169.254",
        // Tailscale's IPv6 range, the twin of 100.64.0.0/10, carved out of
        // the unique-local fc00::/7 above.
        "fd7a:115c:a1e0::1",
        "2001:db8::1",
        "2606:4700::1111",
    ] {
        let addr: IpAddr = internet.parse().expect("an address");
        assert!(
            !listener::is_lan_scope(addr),
            "{internet} is not on decision row 14's list and must read as off-LAN"
        );
    }
}

/// **The gate for the decision**: the old assertion here was `internet_admission`'s own
/// bug written down as a test: "with peer.internet off, anything from
/// anywhere is answered" is exactly `listen: 0.0.0.0:7755` becoming a
/// world-reachable socket on the loosest policy).
///
/// What decides row 14 now is the BIND, not the switch: a listener bound to
/// loopback answers everything, whatever the source, because nothing off this
/// machine can dial it at all. A listener bound to anything else (`0.0.0.0`
/// and a private address such as `10.0.0.5` included, since this node's own
/// mapping keeper asks the router for a mapping on exactly such a bind) is
/// answerable from off the LAN by construction, so from there on only a
/// return visit or an enrolment is answered, independent of `peer.internet`,
/// which [`internet_admission`] reads for one narrowing case only, measured
/// on its own in
/// `a_neighbour_in_this_macs_own_v6_prefix_is_answered_with_the_switch_off`.
/// Every call here passes no prefixes and the switch off, which is the Mac
/// with no global IPv6 at all: the common case, and the one where the bind
/// class decides alone.
#[test]
fn only_ik_is_answered_from_off_the_lan_when_the_bind_is_globally_routable() {
    let off_lan: IpAddr = "2001:db8::1".parse().expect("a documentation address");
    let on_lan: IpAddr = "10.0.0.7".parse().expect("an RFC 1918 address");
    let loopback: IpAddr = "127.0.0.1".parse().expect("loopback");
    let loopback_bind: IpAddr = "127.0.0.1".parse().expect("a loopback bind address");
    let global_bind: IpAddr = "0.0.0.0".parse().expect("an unspecified bind address");

    for pattern in [
        Handshake::Knock,
        Handshake::KnockPsk,
        Handshake::Pair,
        Handshake::Return,
        Handshake::Enrol,
    ] {
        assert_eq!(
            listener::internet_admission(loopback_bind, off_lan, pattern, false, &[]),
            InternetAdmission::Answer,
            "bound to loopback, {pattern:?} from anywhere is answered: nothing off this \
             machine can dial this socket"
        );
        for lan in [on_lan, loopback] {
            assert_eq!(
                listener::internet_admission(global_bind, lan, pattern, false, &[]),
                InternetAdmission::Answer,
                "and on the LAN, {pattern:?} from {lan} is answered even on a globally \
                 routable bind"
            );
        }
    }

    for refused in [Handshake::Knock, Handshake::KnockPsk, Handshake::Pair] {
        assert_eq!(
            listener::internet_admission(global_bind, off_lan, refused, false, &[]),
            InternetAdmission::Refuse,
            "{refused:?} from off the LAN, on a globally routable bind, proves nothing this \
             node issued, so it gets nothing"
        );
    }
    for answered in [Handshake::Return, Handshake::Enrol] {
        assert_eq!(
            listener::internet_admission(global_bind, off_lan, answered, false, &[]),
            InternetAdmission::Answer,
            "{answered:?} carries something this node issued, so it reaches the pin check"
        );
    }
}

/// **The gate for the private bind**: a Mac that binds its own LAN address is
/// reachable from the internet as soon as the mapping keeper gets a mapping,
/// and the keeper asks for one on any bind that is not loopback. The
/// short-circuit that read the bind as "RFC 1918, therefore unreachable"
/// therefore answered a knock and a first pairing from a stranger on exactly
/// the configuration `listen: 10.0.0.5:7755` plus `internet on` produces.
///
/// Watch it fail by putting `is_lan_scope(bind)` back in place of
/// `is_loopback_bind(bind)`: every assertion below flips to `Answer`.
#[test]
fn a_private_bind_refuses_a_knock_from_off_the_lan() {
    let off_lan: IpAddr = "2001:db8::1".parse().expect("a documentation address");
    let on_lan: IpAddr = "10.0.0.7".parse().expect("an RFC 1918 address");

    for private_bind in ["10.0.0.5", "192.168.1.4", "172.16.9.9"] {
        let bind: IpAddr = private_bind.parse().expect("a private bind address");
        for pattern in [Handshake::Knock, Handshake::KnockPsk, Handshake::Pair] {
            for internet in [false, true] {
                assert_eq!(
                    listener::internet_admission(bind, off_lan, pattern, internet, &[]),
                    InternetAdmission::Refuse,
                    "bound to {private_bind}, {pattern:?} from {off_lan} proves nothing this \
                     node issued and a router mapping can carry it here"
                );
            }
        }
        for answered in [Handshake::Return, Handshake::Enrol] {
            assert_eq!(
                listener::internet_admission(bind, off_lan, answered, false, &[]),
                InternetAdmission::Answer,
                "bound to {private_bind}, {answered:?} still reaches the pin check"
            );
        }
        for pattern in [Handshake::Knock, Handshake::Pair] {
            assert_eq!(
                listener::internet_admission(bind, on_lan, pattern, true, &[]),
                InternetAdmission::Answer,
                "and the LAN itself is untouched: {pattern:?} from {on_lan} is answered"
            );
        }
    }
}

/// **The gate for item 2, on the wire.**
///
/// Four connections through the shipped handler, with the switch ON: a knock
/// from `127.0.0.1` and from `10.0.0.7` are answered, a knock from
/// `2001:db8::1` gets zero bytes, and an `IK` from `2001:db8::1` gets past the
/// gate and is refused by the pin check instead.
///
/// The last one is why the refusals are told apart by their REASON and not
/// only by their byte count: an unpinned `IK` writes nothing back either, so
/// "zero bytes" alone cannot tell "refused at the door" from "refused at the
/// pin check", and it is the second that proves the gate let `IK` through.
#[tokio::test(flavor = "multi_thread")]
async fn a_knock_from_off_the_lan_gets_zero_bytes_and_an_ik_reaches_the_pin_check() {
    // `peer.internet` is OFF, and that must no longer matter to what gets
    // answered; the bind below does.
    let node = GateNode::new("gate-on", false);
    let off_lan: SocketAddr = "[2001:db8::1]:44444"
        .parse()
        .expect("a documentation socket");
    let on_lan: SocketAddr = "10.0.0.7:44444".parse().expect("an RFC 1918 socket");
    let loopback: SocketAddr = "127.0.0.1:44444".parse().expect("a loopback socket");
    let global_bind: SocketAddr = "0.0.0.0:7755".parse().expect("an unspecified bind address");

    for lan in [loopback, on_lan] {
        let (_outcome, written) =
            offer_from(node.context(), lan, global_bind, &knock_message_1()).await;
        assert!(
            written > 0,
            "a knock from {lan} is on the LAN and must be answered, whatever the bind"
        );
    }

    let (outcome, written) =
        offer_from(node.context(), off_lan, global_bind, &knock_message_1()).await;
    assert_eq!(
        written, 0,
        "a knock from off the LAN, on a globally routable bind, must get zero bytes back, not \
         a refusal it can read"
    );
    let refusal = outcome.expect_err("a refused knock is an error, not a silent success");
    assert!(
        refusal.contains("globally routable"),
        "and the refusal must name the gate that made it: {refusal}"
    );

    let (outcome, written) = offer_from(
        node.context(),
        off_lan,
        global_bind,
        &ik_message_1_at(&NodeKey::load_or_mint(node.config_dir()).expect("the node's own key")),
    )
    .await;
    assert_eq!(
        written, 0,
        "an unpinned IK writes nothing back either, which is why the reason matters below"
    );
    let refusal = outcome.expect_err("an unpinned IK is refused");
    assert!(
        !refusal.contains("globally routable"),
        "an IK from off the LAN must reach the pin check and be refused THERE, not at the \
         internet gate: {refusal}"
    );
}

/// **The control**: the bug the fix above closes was
/// exactly the assertion this test used to make: "peer.internet is off, so
/// nothing about the source address may change the answer" was true of the
/// SWITCH and false of the SOCKET: a bind that cannot be reached from off the
/// LAN (loopback here) answers a knock from `2001:db8::1` regardless of the
/// switch, because nothing about row 14 needs to apply to a socket a stranger
/// cannot reach in the first place. Watch this fail by reverting
/// `internet_admission` to read `bind` as `is_lan_scope(bind) == false`
/// unconditionally: the knock below would then be refused even on loopback.
#[tokio::test(flavor = "multi_thread")]
async fn a_lan_scoped_bind_answers_the_same_knock_from_off_the_lan_regardless_of_the_switch() {
    let node = GateNode::new("gate-off", false);
    let off_lan: SocketAddr = "[2001:db8::1]:44444"
        .parse()
        .expect("a documentation socket");
    let loopback_bind: SocketAddr = "127.0.0.1:7755".parse().expect("a loopback bind address");

    let (_outcome, written) =
        offer_from(node.context(), off_lan, loopback_bind, &knock_message_1()).await;
    assert!(
        written > 0,
        "a bind that off the LAN cannot reach answers everything, switch or no switch"
    );
}

/// **The real-socket leg**: the accept loop this node actually runs, on the one
/// address this box can dial, with the switch on.
///
/// Everything above hands the source address in by value. This one does not:
/// it binds `127.0.0.1:0`, runs the shipped `serve_on_with`, and dials it, so
/// the gate is proved to be reached through a real accept at least once.
#[tokio::test(flavor = "multi_thread")]
async fn a_real_loopback_knock_is_answered_through_the_accept_loop() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let node = GateNode::new("gate-socket", true);
    let bound = listener::bind("127.0.0.1:0".parse().expect("a loopback address"))
        .await
        .expect("bind a kernel-chosen port on loopback");
    let addr = bound.local_addr().expect("the bound address");
    let context = node.context();
    tokio::spawn(async move {
        listener::serve_on_with(bound, context)
            .await
            .log_debug("serving a test peer listener");
    });

    let message_1 = knock_message_1();
    let mut stream = tokio::net::TcpStream::connect(addr)
        .await
        .expect("dial the listener");
    let mut framed = Vec::with_capacity(2 + message_1.len());
    framed.extend_from_slice(
        &u16::try_from(message_1.len())
            .expect("a short frame")
            .to_be_bytes(),
    );
    framed.extend_from_slice(&message_1);
    stream.write_all(&framed).await.expect("write message 1");
    stream.flush().await.expect("flush message 1");

    let mut back = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut back))
        .await
        .log_debug("waiting for the knock's answer")
        .and_then(|read| read.log_debug("reading the knock's answer"));
    assert!(
        !back.is_empty(),
        "a knock from 127.0.0.1 is on the LAN and must be answered through the real accept \
         loop with peer.internet on"
    );
}

// ---------------------------------------------------------------------------
// The address the far side sees this Mac at
// ---------------------------------------------------------------------------

/// A Hello built for a peer this node saw arrive from a public address carries
/// that address back to it.
///
/// The gate for the learning half of the punch: without this field a Mac
/// behind a NAT has no way at all to know what the rest of the world reaches
/// it at, and the whole punch is two sides dialling addresses neither of them
/// knows.
#[test]
fn a_hello_carries_the_address_this_node_saw_the_peer_from() {
    use tcr::peer::listener::{NodeFacts, hello_for_peer};

    let node = tcr_peer_wire::PeerId([0x71; 32]);
    let seen: SocketAddr = "203.0.113.7:51820".parse().expect("a test address parses");

    let mut facts = NodeFacts::minimal(node);
    facts.observed_peer_at = Some(seen);
    let hello = hello_for_peer(&facts, &Default::default());

    assert_eq!(
        hello.observed_you_at.as_deref(),
        Some("203.0.113.7:51820"),
        "a bare pin is told its own public address, or a Mac behind a NAT never learns one"
    );
}

/// And a node that saw nothing sends no field, rather than an empty string a
/// reader has to decide the meaning of.
#[test]
fn a_hello_from_a_node_that_saw_no_address_carries_no_field() {
    use tcr::peer::listener::{NodeFacts, hello_for_peer};

    let hello = hello_for_peer(
        &NodeFacts::minimal(tcr_peer_wire::PeerId([0x72; 32])),
        &Default::default(),
    );
    assert_eq!(hello.observed_you_at, None, "nothing seen, nothing told");
    let json = serde_json::to_string(&hello).expect("the Hello serializes");
    assert!(
        !json.contains("observedYouAt"),
        "and the key is absent from the wire rather than null: {json}"
    );
}

/// A Hello written by a build that predates the field still parses, and the
/// field reads as "not told".
///
/// The additive claim, checked against bytes rather than against a struct this
/// build could change on both sides at once.
#[test]
fn an_older_hello_without_the_field_still_parses() {
    let node = tcr_peer_wire::PeerId([0x74; 32]).to_wire();
    let older = format!(
        r#"{{"proto":1,"node":"{node}","label":"old","seq":3,
        "caps":{{"egress":false,"forward":false,"lends":false}},"addrs":[],"ttlS":60}}"#
    );
    let hello: tcr_peer_wire::Hello =
        serde_json::from_str(&older).expect("a Hello from an older build still parses");
    assert_eq!(hello.observed_you_at, None, "the field reads as not told");

    let newer = format!(
        r#"{{"proto":1,"node":"{node}","label":"new","seq":3,
        "caps":{{"egress":false,"forward":false,"lends":false}},"addrs":[],"ttlS":60,
        "observedYouAt":"203.0.113.9:4000"}}"#
    );
    let hello: tcr_peer_wire::Hello =
        serde_json::from_str(&newer).expect("and the positive control parses too");
    assert_eq!(
        hello.observed_you_at.as_deref(),
        Some("203.0.113.9:4000"),
        "the control: the same parse DOES read the field when it is there, so the \
         assertion above is about the absent key and not about a parse that drops it"
    );
}

/// Forgetting a Mac takes away the address it saw this one at, along with the
/// pair's rendezvous secret.
///
/// Two ways to meet, one act of forgetting: a register nobody cleared would
/// still hand a punch that Mac's public address for as long as the process
/// lived.
#[test]
fn forgetting_a_peer_takes_away_the_address_it_saw_us_at() {
    let node = tcr_peer_wire::PeerId([0x73; 32]);
    let seen: SocketAddr = "203.0.113.11:7000".parse().expect("a test address parses");
    reach::remember_observed_self(node, seen);
    reach::remember_observed_peer(node, seen);

    assert_eq!(
        reach::observed_self_for(&node),
        Some(seen),
        "the control: the register holds it before the forget"
    );

    assert!(
        reach::forget_port_secret(&node),
        "the forget answers that something was held"
    );
    assert_eq!(
        reach::observed_self_for(&node),
        None,
        "and a forgotten Mac's view of this one goes with it"
    );
    assert_eq!(
        reach::observed_peer_for(&node),
        None,
        "as does this Mac's view of it"
    );
}

// ---------------------------------------------------------------------------
// The home-LAN IPv6 case row 14 got wrong
// ---------------------------------------------------------------------------

/// **The gate for the home-LAN IPv6 regression**: a source in one of this
/// Mac's own `/64` prefixes is answered with the switch OFF, and
/// `2001:db8::1`, outside it, is refused.
///
/// # The case the bind class alone got wrong
///
/// Two Macs on one home network whose ISP delegates a `/64` both hold
/// `2001:...` addresses and both bind `[::]`. Neither address is LAN scope,
/// correctly so: they are globally routable. Row 14 then refused the first
/// pairing and the knock between two Macs on one desk, which is a regression
/// against the same two Macs on IPv4 and is not what row 14 is for.
///
/// Sharing a `/64` with one of this host's own addresses is not authentication
/// and is not treated as any: it reopens the pattern's OWN admission, the
/// operator's pairing window and the knock bucket, exactly as a LAN source
/// does. What it rules out is a stranger anywhere else on the internet.
///
/// The switch ON is the second half and it is the important one: the operator
/// has asked to be reachable from the internet, so row 14 applies in full and
/// the same neighbour is refused.
///
/// Watch it fail: drop the `!internet &&` from `internet_admission`'s refusal
/// arm and the switch-on leg answers; drop the whole
/// `shares_a_global_v6_prefix` arm and the neighbour is refused.
#[test]
fn a_neighbour_in_this_macs_own_v6_prefix_is_answered_with_the_switch_off() {
    let global_bind: IpAddr = "::"
        .parse()
        .expect("the unspecified dual-stack bind address");
    // This Mac's own delegated address, and the Mac on the next desk in the
    // same prefix with a different host part.
    let own: Ipv6Addr = "2001:db8:1234:5678::1"
        .parse()
        .expect("a documentation address");
    let neighbour: IpAddr = "2001:db8:1234:5678:aaaa:bbbb:cccc:dddd"
        .parse()
        .expect("a second address in the same /64");
    let stranger: IpAddr = "2001:db8::1".parse().expect("an address outside that /64");

    for pattern in [Handshake::Knock, Handshake::KnockPsk, Handshake::Pair] {
        assert_eq!(
            listener::internet_admission(global_bind, neighbour, pattern, false, &[own]),
            InternetAdmission::Answer,
            "{pattern:?} from a Mac in this Mac's own /64, with the switch off, is the LAN \
             case wearing a global address: refusing it is the regression this gate exists \
             for"
        );
        assert_eq!(
            listener::internet_admission(global_bind, stranger, pattern, false, &[own]),
            InternetAdmission::Refuse,
            "{pattern:?} from outside every prefix this Mac holds gets nothing, which is the \
             whole of row 14"
        );
        assert_eq!(
            listener::internet_admission(global_bind, neighbour, pattern, true, &[own]),
            InternetAdmission::Refuse,
            "and with `peer.internet` ON the operator asked to be reachable from the \
             internet, so {pattern:?} is refused from the same neighbour"
        );
        assert_eq!(
            listener::internet_admission(global_bind, neighbour, pattern, false, &[]),
            InternetAdmission::Refuse,
            "a Mac with no global IPv6 of its own shares a prefix with nobody, so \
             {pattern:?} is refused exactly as before"
        );
    }

    // The prefix is the first 64 bits and nothing else: the host part differs
    // on every Mac on the link and changes by the hour under privacy
    // extensions.
    assert!(
        listener::shares_a_global_v6_prefix(neighbour, &[own]),
        "the same /64 with a different host part is a neighbour"
    );
    assert!(
        !listener::shares_a_global_v6_prefix(stranger, &[own]),
        "a different /64 is not"
    );
    assert!(
        !listener::shares_a_global_v6_prefix(
            "10.0.0.7".parse().expect("an RFC 1918 address"),
            &[own]
        ),
        "an IPv4 source is the IPv4 question, answered before this one"
    );
    assert!(
        !listener::shares_a_global_v6_prefix(
            "::ffff:10.0.0.7".parse().expect("a v4-mapped address"),
            &[own]
        ),
        "and an IPv4 address wearing a v6 shape is still the IPv4 question, never a /64 \
         comparison against 0:0:0:ffff"
    );
}
