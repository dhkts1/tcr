//! The addresses a join key carries: every address a friend could dial this
//! Mac at, what kind of path each one is, and the order a joiner tries them.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

/// What kind of path one address in a join key is, as the Mac that minted the
/// key knew it.
///
/// **How this Mac learned the address, not a claim on the wire.** The same rule
/// [`crate::peer::config::EndpointSource`] follows, and the reason it is not
/// rendered into the key: a sender's word about its own address is not
/// evidence, the joiner dials in the order it was given, and a reader that
/// believed a kind field would be believing the one party that cannot be
/// checked. It exists so `tcr peer invite` can tell an operator which of the
/// lines under their key is the tailnet one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialAddressKind {
    /// The listener is bound to one specific address and somebody chose it, so
    /// it is the only address the key carries.
    Chosen,
    /// A tailnet address: a `utun` interface holding one out of
    /// `100.64.0.0/10`. First, because it works from any network the friend is
    /// on without a router forwarding anything.
    Tailscale,
    /// The address of tcr's own tailnet node, as the serving process
    /// recorded it (`crate::peer::tailnet_node`): there is no `utun` for it,
    /// so it is read off the state file rather than an interface. First, for
    /// the reason [`Self::Tailscale`] is.
    TailnetNode,
    /// The external socket a held port mapping published, which is the one
    /// address a friend anywhere on the internet can open.
    Internet,
    /// An address a real interface holds: reachable from this network and no
    /// further.
    Lan,
    /// Read out of a pasted key. This Mac derived nothing about it, which is
    /// what a parsed key honestly knows.
    FromKey,
}

impl DialAddressKind {
    /// The one word a surface prints for this kind. One spelling, so an
    /// operator grepping their scrollback and a reader of this file see the
    /// same token.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Chosen => "chosen",
            Self::Tailscale => "tailscale",
            Self::TailnetNode => "tailnet-node",
            Self::Internet => "internet",
            Self::Lan => "lan",
            Self::FromKey => "from-key",
        }
    }
}

/// One address a friend could dial this Mac at, with what kind of path it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DialAddress {
    /// The full socket, as it goes into the key.
    pub addr: SocketAddr,
    /// See [`DialAddressKind`]: local knowledge, never part of the key.
    pub kind: DialAddressKind,
}

impl DialAddress {
    /// An address read out of a pasted key.
    pub const fn from_key(addr: SocketAddr) -> Self {
        Self {
            addr,
            kind: DialAddressKind::FromKey,
        }
    }
}

/// One address an interface holds, with the interface's own name.
///
/// The name is carried because the address alone cannot answer the one question
/// the rank order turns on: `100.64.0.0/10` is both a tailnet and the range a
/// mobile hotspot hands out (`crate::peer::listener::is_lan_scope`'s own doc
/// says so), and putting a hotspot address first would put the slowest path
/// first for everybody on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostAddress {
    /// The interface name, `utun4` or `en0`.
    pub interface: String,
    /// The address it holds.
    pub addr: IpAddr,
}

/// Whether this address is a tailnet one: a `utun` interface holding an
/// address in `100.64.0.0/10`, the range RFC 6598 set aside for carrier-grade
/// NAT and the range Tailscale numbers a tailnet out of.
///
/// **Both halves, and the name is the half that matters.** The range alone also
/// covers a Mac behind a mobile hotspot, whose carrier-NAT address is reachable
/// by nothing outside that hotspot; ranking one of those first would put the
/// deadest address at the top of every key minted on a tethered Mac.
///
/// The range check is [`crate::peer::listener::is_carrier_grade_nat`], the one
/// exported predicate `crate::peer::listener::is_lan_scope_v4` sits beside; an
/// IPv6 address is never a tailnet one here because `is_tailnet` only ever
/// sees the range's own /10, which is IPv4.
fn is_tailnet(host: &HostAddress) -> bool {
    let IpAddr::V4(v4) = host.addr else {
        return false;
    };
    host.interface.starts_with("utun") && crate::peer::listener::is_carrier_grade_nat(v4)
}

/// Every address a friend could dial this Mac at, best first.
///
/// The rank is the order a dial should try, and each band is there because a
/// different friend is on the other end:
///
/// 1. the tailnet address (tcr's own node's, `node`, then any `utun` one), which
///    works from any network without a router forwarding anything,
/// 2. the external socket a held port mapping published, the one address a
///    friend anywhere on the internet can open,
/// 3. every address a real interface holds, for the friend in the next room.
///
/// A pinned `listen` short-circuits all of it: an address somebody typed into
/// the peers file is the address they meant, and a key that quietly carried
/// three others would be answering a question the operator already answered.
///
/// Pure, over a host address list and a mapping, so the order is tested against
/// a written-down set of interfaces rather than against whatever this Mac
/// happens to be plugged into. [`local_dial_addresses`] is the live half.
pub fn dial_addresses(
    listen: SocketAddr,
    host: &[HostAddress],
    external: Option<SocketAddr>,
    node: Option<Ipv4Addr>,
) -> Vec<DialAddress> {
    if listen.port() == 0 {
        return Vec::new();
    }
    if !listen.ip().is_unspecified() {
        return vec![DialAddress {
            addr: listen,
            kind: DialAddressKind::Chosen,
        }];
    }

    let port = listen.port();
    let mut out: Vec<DialAddress> = Vec::new();
    let mut push = |addr: SocketAddr, kind: DialAddressKind| {
        if addr.ip().is_unspecified() || addr.port() == 0 {
            return;
        }
        if out.iter().any(|existing| existing.addr == addr) {
            return;
        }
        out.push(DialAddress { addr, kind });
    };

    // tcr's own tailnet node listens on its address at the same port the
    // kernel listener does (`crate::peer::tailnet_node::Supervisor`).
    if let Some(node) = node {
        push(
            SocketAddr::new(IpAddr::V4(node), port),
            DialAddressKind::TailnetNode,
        );
    }
    for entry in host.iter().filter(|entry| is_tailnet(entry)) {
        push(
            SocketAddr::new(entry.addr, port),
            DialAddressKind::Tailscale,
        );
    }
    if let Some(external) = external {
        push(external, DialAddressKind::Internet);
    }
    for entry in host {
        // The same rule the Peers tab's "is this Mac on a network" fact runs,
        // read from its own module rather than spelled a second time here:
        // loopback, link-local and multicast are addresses no friend can use.
        if is_tailnet(entry) || !crate::status::network_fact::is_usable(entry.addr) {
            continue;
        }
        push(SocketAddr::new(entry.addr, port), DialAddressKind::Lan);
    }
    out
}

/// Every address a real, operationally-up interface holds right now, with the
/// interface's own name.
///
/// The walk is [`crate::status::network_fact::interfaces`], the one place in
/// this tree that calls `if-addrs::get_if_addrs`. An interface whose state
/// this platform cannot report reads as down, which can only ever make a key
/// carry FEWER addresses, never a dead one.
fn host_addresses() -> Vec<HostAddress> {
    crate::status::network_fact::interfaces()
        .into_iter()
        .map(|(interface, addr)| HostAddress { interface, addr })
        .collect()
}

/// [`dial_addresses`] against this Mac's real interfaces and whatever mapping
/// it is holding right now.
///
/// `external` overrides [`crate::peer::reach::external_socket`] when given: a
/// process minting an invite from a shell has run no keeper of its own, so
/// that register is always empty there, and the caller has already read the
/// state file a serving process's keeper wrote
/// ([`crate::peer::reach::recorded_external_socket`]). Only a process that
/// IS the keeper (the register is non-empty) ever needs the fallback.
///
/// `node` is tcr's own tailnet node's address, read the same way off the state
/// file ([`crate::peer::tailnet_node::recorded_node_address`]): the minting
/// process holds no node of its own.
pub fn local_dial_addresses(
    listen: SocketAddr,
    external: Option<SocketAddr>,
    node: Option<Ipv4Addr>,
) -> Vec<DialAddress> {
    dial_addresses(
        listen,
        &host_addresses(),
        external.or_else(crate::peer::reach::external_socket),
        node,
    )
}
