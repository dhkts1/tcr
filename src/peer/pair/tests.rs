use std::net::{IpAddr, Ipv4Addr};

use super::*;

/// The listener's port in every case below. One number, so a rank that
/// changed the port instead of the host would be visible.
const PORT: u16 = 7755;

fn host(interface: &str, addr: &str) -> HostAddress {
    HostAddress {
        interface: interface.to_string(),
        addr: addr.parse().expect("a test address"),
    }
}

fn socket(addr: &str) -> SocketAddr {
    addr.parse().expect("a test socket")
}

fn wide() -> SocketAddr {
    SocketAddr::new(IpAddr::from([0, 0, 0, 0]), PORT)
}

/// The bug this whole shape exists for: a listener on `0.0.0.0` must never
/// put `0.0.0.0` in a key, because the friend then dials their own machine.
#[test]
fn a_wide_listener_never_yields_an_unspecified_address() {
    let addrs = dial_addresses(wide(), &[host("en0", "192.0.2.10")], None, None);
    assert_eq!(
        addrs,
        vec![DialAddress {
            addr: socket("192.0.2.10:7755"),
            kind: DialAddressKind::Lan,
        }],
        "the bind address is not a dial address, and the interface's own is"
    );
    assert!(
        !addrs.iter().any(|entry| entry.addr.ip().is_unspecified()),
        "an unspecified address in a key is the friend dialling their own Mac"
    );
}

/// Somebody typed an address into the peers file, so that is the address
/// they meant and the key carries it alone.
#[test]
fn a_pinned_listener_yields_exactly_that_address() {
    let addrs = dial_addresses(
        socket("192.0.2.10:7755"),
        &[host("en0", "198.51.100.4"), host("utun4", "100.64.0.1")],
        Some(socket("198.51.100.9:41641")),
        Some(Ipv4Addr::new(100, 64, 0, 7)),
    );
    assert_eq!(
        addrs,
        vec![DialAddress {
            addr: socket("192.0.2.10:7755"),
            kind: DialAddressKind::Chosen,
        }],
        "a chosen listen address is an answered question, not a first guess"
    );
}

/// The rank is the dial order: the tailnet address works from anywhere, the
/// mapped external socket works from the internet, a LAN address works in
/// the next room.
#[test]
fn the_rank_is_tailnet_then_internet_then_lan() {
    let addrs = dial_addresses(
        wide(),
        &[
            host("en0", "192.0.2.10"),
            host("utun4", "100.64.0.1"),
            host("lo0", "127.0.0.1"),
            host("en1", "169.254.7.7"),
            host("en0", "2001:db8::4"),
        ],
        Some(socket("198.51.100.9:41641")),
        None,
    );
    assert_eq!(
        addrs
            .iter()
            .map(|entry| (entry.kind, entry.addr.to_string()))
            .collect::<Vec<_>>(),
        vec![
            (DialAddressKind::Tailscale, "100.64.0.1:7755".to_string()),
            (DialAddressKind::Internet, "198.51.100.9:41641".to_string()),
            (DialAddressKind::Lan, "192.0.2.10:7755".to_string()),
            (DialAddressKind::Lan, "[2001:db8::4]:7755".to_string()),
        ],
        "loopback and link-local are addresses no friend can use, and the order is the \
         order a joiner tries"
    );
}

/// **tcr's own tailnet node's address ranks first**, ahead of a `utun`
/// tailnet address, the mapping and the LAN, at the listener's port.
#[test]
fn the_node_address_ranks_first() {
    let addrs = dial_addresses(
        wide(),
        &[host("en0", "192.0.2.10"), host("utun4", "100.64.0.1")],
        Some(socket("198.51.100.9:41641")),
        Some(Ipv4Addr::new(100, 64, 0, 7)),
    );
    assert_eq!(
        addrs
            .iter()
            .map(|entry| (entry.kind, entry.addr.to_string()))
            .collect::<Vec<_>>(),
        vec![
            (DialAddressKind::TailnetNode, "100.64.0.7:7755".to_string()),
            (DialAddressKind::Tailscale, "100.64.0.1:7755".to_string()),
            (DialAddressKind::Internet, "198.51.100.9:41641".to_string()),
            (DialAddressKind::Lan, "192.0.2.10:7755".to_string()),
        ]
    );
}

/// The range alone is not the tailnet test: a tethered Mac holds a
/// carrier-NAT address on a real interface, and ranking that first would
/// put the deadest address at the top of the key.
#[test]
fn a_hotspot_address_is_not_ranked_as_a_tailnet_one() {
    let addrs = dial_addresses(wide(), &[host("en0", "100.64.0.5")], None, None);
    assert_eq!(
        addrs,
        vec![DialAddress {
            addr: socket("100.64.0.5:7755"),
            kind: DialAddressKind::Lan,
        }],
        "the interface name is what separates a tailnet from a carrier NAT"
    );
}

/// A listener with no port yet cannot produce a key at all.
#[test]
fn a_listener_with_no_port_yields_nothing() {
    assert!(
        dial_addresses(
            SocketAddr::new(IpAddr::from([0, 0, 0, 0]), 0),
            &[host("en0", "192.0.2.10")],
            None,
            None,
        )
        .is_empty()
    );
}

/// One address twice on two interfaces is one address in the key: a
/// duplicate costs the joiner a second connect timeout to the same place.
#[test]
fn the_same_address_is_carried_once() {
    let addrs = dial_addresses(
        wide(),
        &[host("en0", "192.0.2.10"), host("bridge0", "192.0.2.10")],
        None,
        None,
    );
    assert_eq!(
        addrs.len(),
        1,
        "a duplicate is a second timeout, not a path"
    );
}

/// A v1 key from an older build still works, byte for byte, when its host
/// is a real one.
#[test]
fn a_v1_key_with_a_real_host_still_parses() {
    let key = format!(
        "{TOKEN_PREFIX}192.0.2.10:7755:{}:{}",
        PeerId([3_u8; 32]).to_wire(),
        encode_key32(&[7_u8; 32])
    );
    let parsed = JoinToken::parse(&key).expect("a v1 key with a real host parses");
    assert_eq!(
        parsed.sockets().collect::<Vec<_>>(),
        vec![socket("192.0.2.10:7755")]
    );
    assert_eq!(parsed.registrar, PeerId([3_u8; 32]));
}

/// And a v1 key minted on a wide listener is refused with the sentence that
/// fixes it, rather than dialled at the joiner's own machine.
#[test]
fn a_v1_key_on_an_unspecified_host_is_refused_with_the_fix() {
    let key = format!(
        "{TOKEN_PREFIX}0.0.0.0:7755:{}:{}",
        PeerId([3_u8; 32]).to_wire(),
        encode_key32(&[7_u8; 32])
    );
    let refusal = JoinToken::parse(&key).expect_err("0.0.0.0 is not an address to dial");
    let text = format!("{refusal:#}");
    assert!(
        text.contains("tcr peer invite"),
        "the refusal has to name the fix, which is a fresh key: {text}"
    );
}

/// One, two, and an IPv6 address: the shapes a v2 key carries.
#[test]
fn a_v2_key_round_trips_every_shape() {
    for addrs in [
        vec![socket("192.0.2.10:7755")],
        vec![socket("100.64.0.1:7755"), socket("198.51.100.9:41641")],
        vec![socket("[2001:db8::4]:7755"), socket("192.0.2.10:7755")],
    ] {
        let token = JoinToken::new(addrs.clone(), PeerId([3_u8; 32]), [7_u8; 32]);
        let rendered = token.to_token();
        assert!(
            rendered.starts_with(TOKEN_PREFIX_V2),
            "a minted key is v2: {rendered}"
        );
        let parsed = JoinToken::parse(&rendered).expect("a v2 key parses");
        assert_eq!(
            parsed.sockets().collect::<Vec<_>>(),
            addrs,
            "the addresses and their ORDER are the dial order: {rendered}"
        );
        assert_eq!(parsed, token, "a key that round trips is the same key");
    }
}

/// A v2 key carrying the bind address is refused for the same reason a v1
/// one is: it is the one address that cannot work.
#[test]
fn a_v2_key_carrying_an_unspecified_address_is_refused() {
    let key = format!(
        "{TOKEN_PREFIX_V2}192.0.2.10:7755,0.0.0.0:7755:{}:{}",
        PeerId([3_u8; 32]).to_wire(),
        encode_key32(&[7_u8; 32])
    );
    assert!(
        JoinToken::parse(&key).is_err(),
        "one unspecified address in the list is still a dial at the joiner's own Mac"
    );
}

/// A share link carrying a key with an IPv6 address survives the round
/// trip, in both directions: the brackets go out escaped, and a link whose
/// brackets a URL layer escaped on the way through still parses.
#[test]
fn a_link_carries_a_key_with_an_ipv6_address_either_way() {
    let network_key = crate::peer::config::NetworkKey::from_bytes([0x2B; 32]);
    let token = JoinToken::new(
        vec![socket("[2001:db8::4]:7755"), socket("192.0.2.10:7755")],
        PeerId([3_u8; 32]),
        [7_u8; 32],
    );
    let link = ShareLink {
        network_key,
        join: Some(token),
    };
    let rendered = link.to_link();
    assert!(
        !rendered.contains('['),
        "a bracket in a query is what the URL layer rewrites: {rendered}"
    );
    assert_eq!(
        ShareLink::parse(&rendered).expect("a rendered link parses"),
        link
    );

    // What the panel really hands over: Foundation escapes the brackets in
    // `absoluteString`, and it may write either case.
    let from_url = rendered.replace("%5B", "%5b").replace("%5D", "%5d");
    assert_eq!(
        ShareLink::parse(&from_url).expect("an escaped link parses"),
        link,
        "the link a click delivers is the one the CLI has to read"
    );
}

/// A version this build does not know is refused rather than guessed at.
///
/// v4 rather than v3, since v3 is a version this build reads now: the
/// fixture moved the moment `JoinToken::parse` grew a v3 arm, the change
/// `tests/peer_noise.rs`'s sibling test moved for too.
#[test]
fn an_unknown_version_is_refused() {
    let key = format!(
        "tcr-join:v4:192.0.2.10:7755:{}:{}",
        PeerId([3_u8; 32]).to_wire(),
        encode_key32(&[7_u8; 32])
    );
    let refusal = JoinToken::parse(&key).expect_err("v4 is not a version this build reads");
    assert!(
        format!("{refusal:#}").contains("not a join key"),
        "the refusal is about the VERSION and not about a field further in"
    );
}

/// A v3 key whose magic bytes are missing is refused for that, not
/// silently read as if the magic matched.
#[test]
fn a_v3_key_with_the_wrong_magic_is_refused() {
    let bytes = encode_bytes(&[0_u8; 70]);
    let key = format!("{TOKEN_PREFIX_V3}{bytes}");
    let refusal = JoinToken::parse(&key).expect_err("the wrong magic must be refused");
    assert!(
        format!("{refusal:#}").contains("magic"),
        "the refusal names what was wrong: {refusal:#}"
    );
}

/// **A recorded mapping the caller hands over reaches the token as an
/// Internet entry, ranked ahead of every LAN address** this test machine
/// happens to hold, not a second guess `mint_invite_as` makes on its own.
///
/// The whole reason `external` is a parameter rather than another read of
/// `reach::external_socket` inside this function: `tcr peer invite` runs
/// as its own process, with no keeper thread to fill that register, so the
/// only way a live router mapping ever reaches this token is a caller that
/// already read it off the state file and handed it in. The rank check
/// stops at "ahead of every LAN entry" rather than "position zero"
/// because a real interface list is not this test's to control: a Mac
/// running Tailscale ranks its tailnet address first regardless, by
/// [`dial_addresses`]'s own documented order.
/// **An invite minted on a Mac in node mode lists the node's address
/// first**, read off the state file the serving process wrote, and an
/// invite minted with no fresh node record carries none.
///
/// Watch it fail by passing `None` for the node in `mint_invite_as`: the
/// first address is then an interface's, not `100.64.0.7`.
#[test]
fn an_invite_minted_in_node_mode_lists_the_node_address_first() {
    let dir = tempfile::TempDir::new().expect("a scratch dir");
    let peers_path = dir.path().join("tcr-peers.json");
    let file = PeerFile {
        listen: Some(socket("0.0.0.0:7755")),
        tailnet: crate::peer::config::TailnetMode::Node.into(),
        ..PeerFile::default()
    };
    save(&peers_path, &file).expect("the peers file writes");
    let store = PeerStore::open(&peers_path).expect("the store opens");
    let node = NodeKey::load_or_mint(dir.path()).expect("a node key");

    let (_invite, token) =
        mint_invite_as(&store, &node, "laptop-2", 600, 1, None).expect("an invite");
    assert!(
        token
            .addrs
            .iter()
            .all(|entry| entry.kind != DialAddressKind::TailnetNode),
        "no record, no node address: {:?}",
        token.addrs
    );

    crate::peer::state::save_tailnet(
        &crate::peer::state::peer_state_path(&peers_path),
        crate::peer::state::TailnetRecord {
            mode: crate::peer::config::TailnetMode::Node.into(),
            state: crate::peer::state::TailnetState::Ready,
            address: Some(Ipv4Addr::new(100, 64, 0, 7)),
            node_name: Some("tcr-studio".to_string()),
            found: 0,
            connected: 0,
            detail: None,
            recorded_at_ms: now_ms(),
        },
        Vec::new(),
    )
    .expect("the record writes");
    let (_invite, token) =
        mint_invite_as(&store, &node, "laptop-3", 600, 1, None).expect("an invite");
    assert_eq!(
        token.addrs.first().map(|entry| (entry.kind, entry.addr)),
        Some((DialAddressKind::TailnetNode, socket("100.64.0.7:7755"))),
        "{:?}",
        token.addrs
    );
}

#[test]
fn mint_invite_carries_a_handed_in_mapping_as_an_internet_entry() {
    let dir = tempfile::TempDir::new().expect("a scratch dir");
    let peers_path = dir.path().join("tcr-peers.json");
    let file = PeerFile {
        listen: Some(socket("0.0.0.0:7755")),
        ..PeerFile::default()
    };
    save(&peers_path, &file).expect("the peers file writes");

    let store = PeerStore::open(&peers_path).expect("the store opens");
    let node = NodeKey::load_or_mint(dir.path()).expect("a node key");
    let external = socket("198.51.100.9:7755");

    let (_invite, token) = mint_invite_as(&store, &node, "laptop-2", 600, 1, Some(external))
        .expect("an invite is minted");

    let internet_rank = token
        .addrs
        .iter()
        .position(|entry| entry.addr == external && entry.kind == DialAddressKind::Internet)
        .unwrap_or_else(|| {
            panic!(
                "the handed-in socket is missing or not marked Internet: {:?}",
                token.addrs
            )
        });
    for (rank, entry) in token.addrs.iter().enumerate() {
        if entry.kind == DialAddressKind::Lan {
            assert!(
                internet_rank < rank,
                "the recorded mapping must outrank every LAN address, the one a friend \
                 off this LAN cannot dial: {:?}",
                token.addrs
            );
        }
    }
}
