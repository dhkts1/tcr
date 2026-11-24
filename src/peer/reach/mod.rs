//! What this Mac can be reached on from somewhere that is not this LAN.
//!
//! Nothing here decides trust. Every address and port below is ROUTING ADVICE,
//! exactly as [`crate::peer::config::PeerRow`] says of the addresses it stores:
//! identity stays the pinned static key and is re-proven by the handshake every
//! time, so a forged mapping or a guessed port buys an attacker a connection
//! that then fails the pin check.
//!
//! **A port on the router.** [`NatPmp`] asks the gateway for one, in the 12
//! bytes RFC 6886 defines. Hand-written rather than a crate: the whole protocol
//! is two request shapes and two response shapes, sent as UDP to one host on
//! the local link.
//!
//! **An address that needs no router at all.** [`global_v6_addresses`]. A
//! global IPv6 address is reachable directly, so two Macs that both have one
//! need no mapping, no relay and no hole punch.
//!
//! # Why not a STUN client
//!
//! A reflexive address (RFC 8489) tells this Mac what a third party sees it
//! from, which is the right answer to a different question and needs a third
//! party. Everything here is answerable from this machine and its own router,
//! so `tcr peer reach` works on a mesh of exactly two Macs with nothing
//! deployed anywhere.
//!
//! # The derived port is dial-only today
//!
//! [`accepted_ports`] and [`port_is_accepted`] answer "what port would the
//! other end of a completed handshake accept from us in this slot", which is
//! exactly the question a DIALLER asks before it tries a rendezvous port a
//! recorded endpoint stopped answering on ([`rendezvous_ports`], the caller
//! `tests/peer_reach.rs` exercises). Nothing in this repository BINDS on a
//! derived port or checks an inbound connection's source port against
//! [`port_is_accepted`]: [`crate::peer::listener::serve`] binds the one
//! configured `listen` address, and that is the only socket this node ever
//! answers on. A reader is not meant to infer a listener side from a
//! function's mere existence; there is none, and `port_is_accepted` is kept
//! for the day a listener-side check is added, not because one runs now.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context as _, Result};

/// The UDP port every NAT-PMP gateway listens on (RFC 6886 § 3).
pub const GATEWAY_PORT: u16 = 5351;

/// The only protocol version RFC 6886 defines, and the only one a gateway may
/// answer: a response carrying anything else is refused rather than guessed at,
/// because the layout of every field after the version byte is what the version
/// selects.
pub const VERSION: u8 = 0;

/// Opcode 0, "tell me my external address" (RFC 6886 § 3.2).
const OP_EXTERNAL: u8 = 0;
/// Opcode 1, map a UDP port (RFC 6886 § 3.3).
const OP_MAP_UDP: u8 = 1;
/// Opcode 2, map a TCP port (RFC 6886 § 3.3).
const OP_MAP_TCP: u8 = 2;
/// A response carries the request's opcode with the top bit set (RFC 6886 § 3).
const RESPONSE_BIT: u8 = 0x80;

/// RFC 6886 § 3.1: wait 250 ms for the first reply, then double.
const FIRST_TIMEOUT: Duration = Duration::from_millis(250);

/// How many times a request is sent before the gateway is called silent.
///
/// The RFC allows nine, doubling to about a minute. Three is the number for an
/// interactive verb: 250 ms + 500 ms + 1 s is 1.75 s worst case, and a gateway
/// that has not answered three times in 1.75 s on the local link is not a
/// gateway that speaks NAT-PMP. A silent gateway is a normal outcome, most
/// routers ship with this off, so it is an error value, never a panic.
pub const TRIES: u32 = 3;

/// The largest response any opcode here produces, so one buffer serves all of
/// them: 16 bytes for a mapping response (RFC 6886 § 3.3).
const MAX_RESPONSE: usize = 16;

/// Which transport a mapping is for.
///
/// A typed pair rather than the wire's opcode byte: the byte appears once, in
/// [`Self::request_opcode`], and a caller that had to remember "1 is UDP" is a
/// caller that maps TCP into the UDP table once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapProtocol {
    /// Opcode 1.
    Udp,
    /// Opcode 2.
    Tcp,
}

impl MapProtocol {
    /// The request opcode for this protocol.
    const fn request_opcode(self) -> u8 {
        match self {
            Self::Udp => OP_MAP_UDP,
            Self::Tcp => OP_MAP_TCP,
        }
    }

    /// The name used in output a human reads.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Udp => "udp",
            Self::Tcp => "tcp",
        }
    }
}

/// The gateway's verdict on one request (RFC 6886 § 3.5).
///
/// [`Self::Unknown`] rather than a refusal on an unrecognised number: the RFC
/// reserves the space and a future gateway may use it, and "the router said 7"
/// is a more useful thing to print than "malformed".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultCode {
    /// 0.
    Success,
    /// 1: the gateway does not speak version 0.
    UnsupportedVersion,
    /// 2: NAT-PMP is off, or this host is not allowed to ask. **The common
    /// one**: most routers ship with port mapping disabled.
    NotAuthorized,
    /// 3: the gateway itself has no external address yet.
    NetworkFailure,
    /// 4: the gateway is out of mapping table entries.
    OutOfResources,
    /// 5: the gateway does not know this opcode.
    UnsupportedOpcode,
    /// Anything else the gateway sent.
    Unknown(u16),
}

impl ResultCode {
    /// Read a result code off the wire.
    const fn from_wire(code: u16) -> Self {
        match code {
            0 => Self::Success,
            1 => Self::UnsupportedVersion,
            2 => Self::NotAuthorized,
            3 => Self::NetworkFailure,
            4 => Self::OutOfResources,
            5 => Self::UnsupportedOpcode,
            other => Self::Unknown(other),
        }
    }
}

impl std::fmt::Display for ResultCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Success => f.write_str("success"),
            Self::UnsupportedVersion => f.write_str("unsupported NAT-PMP version"),
            Self::NotAuthorized => {
                f.write_str("not authorized (port mapping is off on the router)")
            }
            Self::NetworkFailure => f.write_str("the router has no external address"),
            Self::OutOfResources => f.write_str("the router's mapping table is full"),
            Self::UnsupportedOpcode => f.write_str("the router does not know this request"),
            Self::Unknown(code) => write!(f, "result code {code}"),
        }
    }
}

/// Everything that can stop a reachability probe, each one a value a caller
/// prints rather than a condition it panics on.
#[derive(Debug, thiserror::Error)]
pub enum ReachError {
    /// No default route with a gateway address on it.
    #[error("peer reach: no default IPv4 gateway found ({0})")]
    NoGateway(String),
    /// The local socket would not open, bind, send or receive.
    #[error("peer reach: the local UDP socket failed: {0}")]
    Socket(#[source] std::io::Error),
    /// The gateway never answered. **Not a failure of this Mac**: a router with
    /// NAT-PMP off usually drops the packet rather than refusing it.
    #[error("peer reach: the gateway {gateway} did not answer after {tries} tries")]
    Silent {
        /// Who was asked.
        gateway: SocketAddr,
        /// How many times.
        tries: u32,
    },
    /// The gateway refused the packet rather than answering it: an ICMP port
    /// unreachable, which arrives here as `ConnectionRefused`, or the reset
    /// some gateways send instead.
    ///
    /// **Not a fault of the local socket**, which is what this used to be
    /// reported as, and what it cost: a router with no NAT-PMP service on port
    /// [`GATEWAY_PORT`] refuses the probe, the refusal came back as
    /// [`Self::Socket`], and [`MappingKeeper::map`] asks UPnP only about an
    /// answer that means "this router may not speak NAT-PMP at all". So a
    /// router that speaks UPnP and not NAT-PMP was never asked over UPnP, and
    /// `tcr peer reach` told its operator the local UDP socket had failed.
    #[error(
        "peer reach: the gateway {gateway} refused the NAT-PMP probe, so nothing there speaks \
         it ({source})"
    )]
    NoNatPmp {
        /// Who refused.
        gateway: SocketAddr,
        /// What the socket reported.
        #[source]
        source: std::io::Error,
    },
    /// The gateway answered and said no.
    #[error("peer reach: the gateway refused: {0}")]
    Refused(ResultCode),
    /// The gateway answered with something that is not a NAT-PMP response.
    #[error("peer reach: the gateway's answer was not readable: {0}")]
    Malformed(String),
}

impl ReachError {
    /// Does this answer mean "there is no NAT-PMP service on that router"?
    ///
    /// Two shapes, one meaning. A router with the service off usually drops the
    /// probe ([`Self::Silent`]); one with nothing bound on [`GATEWAY_PORT`]
    /// refuses it ([`Self::NoNatPmp`]). Both are answers about the ROUTER and
    /// neither is a decision the router made about this node, which is what
    /// separates them from [`Self::Refused`]: a gateway that read the request
    /// and said no has decided, and asking a second protocol would be routing
    /// around it.
    ///
    /// One predicate rather than a `matches!` at each site, because the two
    /// sites are the UPnP fallback and the retry ladder, and a router the
    /// fallback treats as mapless while the ladder treats it as broken is a
    /// router asked over UPnP every five seconds forever.
    pub const fn no_natpmp_service(&self) -> bool {
        matches!(self, Self::Silent { .. } | Self::NoNatPmp { .. })
    }
}

/// The gateway's own view of the internet, as opcode 0 reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExternalAddress {
    /// The address the world sees this network as.
    pub addr: Ipv4Addr,
    /// The gateway's seconds-since-start-of-epoch counter. Kept because a
    /// DECREASE in it across two probes is how RFC 6886 § 3.6 says a client
    /// learns the router rebooted and dropped every mapping it holds.
    pub epoch_secs: u32,
}

/// One mapping, as the gateway granted it, never as it was asked for.
///
/// Every field is the gateway's answer and not the request's wish: RFC 6886
/// § 3.3 lets a gateway hand back a different external port and a shorter
/// lifetime than the one asked for, and a caller that assumed its own numbers
/// would advertise a port nothing is listening behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mapping {
    /// Which table the mapping is in.
    pub protocol: MapProtocol,
    /// The port on this Mac.
    pub internal_port: u16,
    /// The port on the router. **This is the one to advertise.**
    pub external_port: u16,
    /// How long the gateway says it will hold it, in seconds.
    pub lifetime_secs: u32,
    /// The gateway's epoch counter, for the reboot check in
    /// [`ExternalAddress::epoch_secs`].
    pub epoch_secs: u32,
}

/// A NAT-PMP client pointed at one gateway.
///
/// Synchronous on purpose, though the rest of this crate is async. The only
/// caller is a CLI verb that has nothing else in flight, and a blocking
/// [`UdpSocket`] with a read timeout is drivable from a plain thread, which is
/// what lets the test suite stand a fake gateway on loopback with no runtime
/// and no network.
#[derive(Debug, Clone, Copy)]
pub struct NatPmp {
    gateway: SocketAddr,
}

impl NatPmp {
    /// Point a client at a gateway address, for a test's fake responder or an
    /// operator who knows better than the route table.
    pub const fn at(gateway: SocketAddr) -> Self {
        Self { gateway }
    }

    /// Point a client at this machine's default IPv4 gateway.
    pub fn on_default_gateway() -> Result<Self, ReachError> {
        let gateway = default_gateway()?;
        Ok(Self::at(SocketAddr::new(IpAddr::V4(gateway), GATEWAY_PORT)))
    }

    /// Who this client talks to.
    /// This Mac's own address on the LAN the gateway is on, which is what a
    /// UPnP `AddPortMapping` names as the internal client.
    ///
    /// The kernel's own source-address selection, read back off a connected UDP
    /// socket, the same trick and for the same reason as
    /// [`global_v6_addresses`]: `connect` on a datagram socket sends NOTHING,
    /// it records a destination and runs the route lookup, so this touches no
    /// network. NAT-PMP never needs it (the gateway reads the source address off
    /// the packet), which is why it arrives with the UPnP fallback and not
    /// before.
    pub fn internal_address(&self) -> Result<Ipv4Addr, ReachError> {
        let socket = UdpSocket::bind("0.0.0.0:0").map_err(ReachError::Socket)?;
        socket.connect(self.gateway).map_err(ReachError::Socket)?;
        match socket.local_addr().map_err(ReachError::Socket)? {
            SocketAddr::V4(local) => Ok(*local.ip()),
            SocketAddr::V6(local) => Err(ReachError::Malformed(format!(
                "peer internet: the route to {} is v6 ({local}) and a UPnP internal client is an IPv4 address",
                self.gateway
            ))),
        }
    }

    pub const fn gateway(&self) -> SocketAddr {
        self.gateway
    }

    /// Opcode 0: ask the gateway what the world sees this network as.
    pub fn external_address(&self) -> Result<ExternalAddress, ReachError> {
        let response = self.exchange(&[VERSION, OP_EXTERNAL], OP_EXTERNAL, 12)?;
        // Bytes 8..12 are the external IPv4 address (RFC 6886 § 3.2).
        let octets: [u8; 4] = response[8..12]
            .try_into()
            .map_err(|_| ReachError::Malformed("the external address field was short".into()))?;
        Ok(ExternalAddress {
            addr: Ipv4Addr::from(octets),
            epoch_secs: epoch_secs(&response)?,
        })
    }

    /// Opcode 1 or 2: ask for a mapping from an external port to `internal_port`
    /// on this Mac, held for `lifetime_secs`.
    ///
    /// `suggested_external` is a wish, not a reservation. Pass `internal_port`
    /// for the RFC's recommended first attempt, or `0` to let the gateway pick;
    /// either way the port to advertise is the one in the returned
    /// [`Mapping::external_port`].
    pub fn map(
        &self,
        protocol: MapProtocol,
        internal_port: u16,
        suggested_external: u16,
        lifetime_secs: u32,
    ) -> Result<Mapping, ReachError> {
        let opcode = protocol.request_opcode();
        let mut request = Vec::with_capacity(12);
        request.push(VERSION);
        request.push(opcode);
        // Two reserved bytes, which RFC 6886 § 3.3 requires be zero.
        request.extend_from_slice(&0_u16.to_be_bytes());
        request.extend_from_slice(&internal_port.to_be_bytes());
        request.extend_from_slice(&suggested_external.to_be_bytes());
        request.extend_from_slice(&lifetime_secs.to_be_bytes());

        let response = self.exchange(&request, opcode, 16)?;
        let granted_internal = be_u16(&response, 8)?;
        let granted_external = be_u16(&response, 10)?;
        let granted_lifetime = be_u32(&response, 12)?;
        Ok(Mapping {
            protocol,
            internal_port: granted_internal,
            external_port: granted_external,
            lifetime_secs: granted_lifetime,
            epoch_secs: epoch_secs(&response)?,
        })
    }

    /// Re-ask for the mapping this Mac already holds, extending its lifetime.
    ///
    /// The same request as [`Self::map`], which is the whole renewal protocol:
    /// RFC 6886 § 3.3 says a repeated request for an existing mapping refreshes
    /// it rather than adding a second one. Re-asks for the external port the
    /// gateway actually granted, so a renewal cannot silently move the port
    /// every peer was told about.
    pub fn renew(&self, mapping: &Mapping, lifetime_secs: u32) -> Result<Mapping, ReachError> {
        self.map(
            mapping.protocol,
            mapping.internal_port,
            mapping.external_port,
            lifetime_secs,
        )
    }

    /// Drop a mapping: RFC 6886 § 3.3's lifetime 0 with a suggested external
    /// port of 0.
    ///
    /// Returns the gateway's response so a caller can log what it confirmed.
    /// The RFC requires the external port in a delete response to be 0, and a
    /// gateway that answers otherwise has not deleted what was asked.
    pub fn delete(&self, protocol: MapProtocol, internal_port: u16) -> Result<Mapping, ReachError> {
        let confirmed = self.map(protocol, internal_port, 0, 0)?;
        if confirmed.external_port != 0 || confirmed.lifetime_secs != 0 {
            return Err(ReachError::Malformed(format!(
                "a delete was confirmed with external port {} and lifetime {}, and RFC 6886 \
                 requires both to be zero",
                confirmed.external_port, confirmed.lifetime_secs
            )));
        }
        Ok(confirmed)
    }

    /// Send `request` up to [`TRIES`] times and return the first well-formed
    /// response for `request_opcode`.
    ///
    /// Three things are checked before the bytes are handed back, and each one
    /// is a way a reply can look fine and mean something else: the version (the
    /// field layout depends on it), the opcode (a gateway answering an earlier
    /// request of ours would otherwise be read as answering this one), and the
    /// result code (a refusal carries a well-formed header and zeroes in every
    /// field a caller would go on to read).
    fn exchange(
        &self,
        request: &[u8],
        request_opcode: u8,
        expected_len: usize,
    ) -> Result<Vec<u8>, ReachError> {
        let bind: SocketAddr = match self.gateway {
            SocketAddr::V4(_) => "0.0.0.0:0".parse(),
            SocketAddr::V6(_) => "[::]:0".parse(),
        }
        .map_err(|err: std::net::AddrParseError| {
            ReachError::Malformed(format!("the bind address would not parse: {err}"))
        })?;
        let socket = UdpSocket::bind(bind).map_err(ReachError::Socket)?;
        socket.connect(self.gateway).map_err(ReachError::Socket)?;

        let mut timeout = FIRST_TIMEOUT;
        for _ in 0..TRIES {
            socket
                .set_read_timeout(Some(timeout))
                .map_err(ReachError::Socket)?;
            // Both halves are classified, because a refusal can land on
            // either: the ICMP port unreachable for one send is delivered to
            // the next operation on the connected socket, which is this recv
            // on the same pass and this send on the following one.
            socket.send(request).map_err(|err| self.probe_error(err))?;

            let mut buffer = [0_u8; MAX_RESPONSE];
            match socket.recv(&mut buffer) {
                Ok(read) => {
                    let response = &buffer[..read];
                    return interpret(response, request_opcode, expected_len);
                }
                Err(err)
                    if matches!(
                        err.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    // RFC 6886 § 3.1: double and try again.
                    timeout *= 2;
                }
                Err(err) => return Err(self.probe_error(err)),
            }
        }
        Err(ReachError::Silent {
            gateway: self.gateway,
            tries: TRIES,
        })
    }

    /// Name a socket error from the [`GATEWAY_PORT`] exchange for what it says
    /// about the ROUTER, where it says anything.
    ///
    /// A refused datagram and a reset are the gateway's answer, not this Mac's
    /// failure; everything else is the local socket and keeps its old name.
    fn probe_error(&self, err: std::io::Error) -> ReachError {
        match err.kind() {
            std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::ConnectionReset => {
                ReachError::NoNatPmp {
                    gateway: self.gateway,
                    source: err,
                }
            }
            _ => ReachError::Socket(err),
        }
    }
}

/// Check a NAT-PMP response's header and length, and refuse it otherwise.
///
/// Free of the socket so the suite can drive every refusal shape, a short
/// frame, the wrong version, another opcode's answer, a non-zero result code,
/// without standing up a responder for each one.
pub fn interpret(
    response: &[u8],
    request_opcode: u8,
    expected_len: usize,
) -> Result<Vec<u8>, ReachError> {
    if response.len() < expected_len {
        return Err(ReachError::Malformed(format!(
            "expected at least {expected_len} bytes, got {}",
            response.len()
        )));
    }
    if response[0] != VERSION {
        return Err(ReachError::Malformed(format!(
            "expected version {VERSION}, got {}",
            response[0]
        )));
    }
    let expected_opcode = request_opcode | RESPONSE_BIT;
    if response[1] != expected_opcode {
        return Err(ReachError::Malformed(format!(
            "expected opcode {expected_opcode}, got {}",
            response[1]
        )));
    }
    let code = ResultCode::from_wire(be_u16(response, 2)?);
    if code != ResultCode::Success {
        return Err(ReachError::Refused(code));
    }
    Ok(response[..expected_len].to_vec())
}

/// Bytes 4..8 of every response: the gateway's epoch counter.
fn epoch_secs(response: &[u8]) -> Result<u32, ReachError> {
    be_u32(response, 4)
}

/// A big-endian `u16` at `offset`, or a refusal naming the offset.
fn be_u16(bytes: &[u8], offset: usize) -> Result<u16, ReachError> {
    let slice = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| ReachError::Malformed(format!("no u16 at byte {offset}")))?;
    let pair: [u8; 2] = slice
        .try_into()
        .map_err(|_| ReachError::Malformed(format!("no u16 at byte {offset}")))?;
    Ok(u16::from_be_bytes(pair))
}

/// A big-endian `u32` at `offset`, or a refusal naming the offset.
fn be_u32(bytes: &[u8], offset: usize) -> Result<u32, ReachError> {
    let slice = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| ReachError::Malformed(format!("no u32 at byte {offset}")))?;
    let quad: [u8; 4] = slice
        .try_into()
        .map_err(|_| ReachError::Malformed(format!("no u32 at byte {offset}")))?;
    Ok(u32::from_be_bytes(quad))
}

/// This Mac's default gateway, or `None` when it has none or it could not be
/// read: the network-change signal tcr's own tailnet node
/// (`crate::peer::tailnet_node`) shares with the mapping keeper. Blocking: it
/// runs a route tool on macOS.
pub fn default_gateway_ipv4() -> Option<Ipv4Addr> {
    default_gateway().ok()
}

/// This machine's default IPv4 gateway.
///
/// Two sources, in this order, because on this box the first one alone finds
/// nothing. `route -n get default` reports the route the kernel would actually
/// take, which on a Mac with a VPN up is a `utun` interface, and a point-to-point
/// interface's route has NO gateway address at all, so the output carries
/// `interface: utun4` and no `gateway:` line. `netstat -rn -f inet` lists every
/// default route, including the physical one through the LAN router, which is the
/// only one a NAT-PMP request could ever be answered by.
fn default_gateway() -> Result<Ipv4Addr, ReachError> {
    #[cfg(target_os = "macos")]
    {
        if let Some(found) = run_route_tool(&["route", "-n", "get", "default"], parse_route_get) {
            return Ok(found);
        }
        if let Some(found) = run_route_tool(&["netstat", "-rn", "-f", "inet"], parse_netstat_inet) {
            return Ok(found);
        }
        Err(ReachError::NoGateway(
            "neither `route -n get default` nor `netstat -rn -f inet` named one".into(),
        ))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let table = std::fs::read_to_string("/proc/net/route")
            .map_err(|err| ReachError::NoGateway(format!("/proc/net/route: {err}")))?;
        parse_proc_net_route(&table)
            .ok_or_else(|| ReachError::NoGateway("/proc/net/route lists no default route".into()))
    }
}

/// Run one route-table tool and hand its stdout to `parse`.
///
/// A missing tool, a non-zero exit and unreadable output are all one outcome
/// here, [`None`], so the caller falls through to the next source, because
/// there is a second source and the operator gets one refusal naming both.
#[cfg(target_os = "macos")]
fn run_route_tool(argv: &[&str], parse: fn(&str) -> Option<Ipv4Addr>) -> Option<Ipv4Addr> {
    let (program, args) = argv.split_first()?;
    let output = std::process::Command::new(program)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse(&String::from_utf8_lossy(&output.stdout))
}

/// The `gateway:` line of `route -n get default`.
///
/// Returns [`None`] when there is no such line, which is the normal shape for a
/// default route over a point-to-point interface. See [`default_gateway`].
pub fn parse_route_get(output: &str) -> Option<Ipv4Addr> {
    output
        .lines()
        .filter_map(|line| line.trim().strip_prefix("gateway:"))
        .find_map(|value| value.trim().parse::<Ipv4Addr>().ok())
}

/// The first `default` row of `netstat -rn -f inet` whose gateway is an address.
///
/// The rows whose gateway reads `link#24` are the point-to-point ones, and they
/// are skipped by the parse rather than by a special case: a gateway that is not
/// an IPv4 address is not somewhere a 12-byte UDP request can be sent.
pub fn parse_netstat_inet(output: &str) -> Option<Ipv4Addr> {
    output.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        if fields.next() != Some("default") {
            return None;
        }
        fields.next()?.parse::<Ipv4Addr>().ok()
    })
}

/// The gateway of the default route in a `/proc/net/route` table (Linux).
///
/// The columns are `Iface Destination Gateway Flags ...` and every address is
/// hex in the HOST's byte order, which on every platform this ships to is
/// little-endian, so `0101A8C0` is 192.168.1.1 and reading it big-endian would
/// give 1.1.168.192, a plausible-looking address pointing nowhere.
pub fn parse_proc_net_route(table: &str) -> Option<Ipv4Addr> {
    table.lines().skip(1).find_map(|line| {
        let mut fields = line.split_whitespace();
        let _iface = fields.next()?;
        let destination = fields.next()?;
        let gateway = fields.next()?;
        if destination != "00000000" {
            return None;
        }
        let raw = u32::from_str_radix(gateway, 16).ok()?;
        let addr = Ipv4Addr::from(raw.to_le_bytes());
        if addr.is_unspecified() {
            return None;
        }
        Some(addr)
    })
}

/// Whether an IPv6 address is one the open internet can route to this Mac on.
///
/// Stated as a list of exclusions taken from the registry rather than as
/// "starts with 2 or 3": `2000::/3` is today's global unicast assignment and
/// not a permanent definition, and the address that matters most to get right
/// here is the one a VPN hands out, a unique-local `fd00::/8`, which looks
/// like an address, answers a bind, and is unreachable from anywhere else.
pub fn is_global_v6(addr: &Ipv6Addr) -> bool {
    let segments = addr.segments();
    // Unspecified, loopback and multicast are not unicast addresses at all.
    if addr.is_unspecified() || addr.is_loopback() || addr.is_multicast() {
        return false;
    }
    // fe80::/10 link-local: scoped to one link, and the scope id is not
    // something another Mac can be told.
    if segments[0] & 0xffc0 == 0xfe80 {
        return false;
    }
    // fc00::/7 unique-local (RFC 4193). Tailscale and most VPNs hand these out.
    if segments[0] & 0xfe00 == 0xfc00 {
        return false;
    }
    // 2001:db8::/32, reserved for documentation (RFC 3849).
    if segments[0] == 0x2001 && segments[1] == 0x0db8 {
        return false;
    }
    // ::ffff:0:0/96 v4-mapped and ::/96 v4-compatible: an IPv4 address wearing
    // a v6 shape, whose reachability is the IPv4 question and not this one.
    if addr.to_ipv4_mapped().is_some() || addr.to_ipv4().is_some() {
        return false;
    }
    // 100::/64, discard-only (RFC 6666).
    if segments[0] == 0x0100 && segments[1..4] == [0, 0, 0] {
        return false;
    }
    true
}

/// The global IPv6 addresses this Mac would send from.
///
/// # What this returns and what it does not
///
/// The kernel's own source-address selection, read back off a connected UDP
/// socket. `connect` on a datagram socket sends NOTHING: it records a
/// destination and runs the route lookup, so this touches no network, and the
/// destinations asked about are `2001:db8::1` (reserved for documentation, RFC
/// 3849) and `2001:4860:4860::8888`, a public resolver, so a box with a default
/// v6 route but no route for the documentation prefix still answers.
///
/// It is therefore the address this Mac would be SEEN from, which is the one a
/// peer needs, and **not** the full interface list. A Mac with two global
/// addresses on two interfaces reports the preferred one. The full list needs
/// `getifaddrs`, which is C and cannot be called from this crate
/// (`#![forbid(unsafe_code)]`, `src/lib.rs:8`); the crate that wraps it,
/// `if-addrs`, is already resolved in `Cargo.lock` through `mdns-sd`, so it
/// would add a dependency EDGE and no package, an ask recorded for the lead
/// rather than taken.
///
/// An empty list is a normal, common answer: measured on this machine, whose
/// only non-link-local v6 addresses are two unique-local ones from VPN
/// interfaces.
pub fn global_v6_addresses() -> Vec<Ipv6Addr> {
    /// Destinations whose route lookup reveals a source address. Both are
    /// off-link on purpose: a link-local destination would select a link-local
    /// source, which is exactly the answer this function must not give.
    const PROBES: [&str; 2] = ["[2001:db8::1]:9", "[2001:4860:4860::8888]:9"];

    let mut found: Vec<Ipv6Addr> = Vec::new();
    for probe in PROBES {
        let Ok(destination) = probe.parse::<SocketAddr>() else {
            continue;
        };
        let Ok(socket) = UdpSocket::bind("[::]:0") else {
            continue;
        };
        if socket.connect(destination).is_err() {
            continue;
        }
        let Ok(SocketAddr::V6(local)) = socket.local_addr() else {
            continue;
        };
        let addr = *local.ip();
        if is_global_v6(&addr) && !found.contains(&addr) {
            found.push(addr);
        }
    }
    found
}

mod mapping;
mod observed;
mod port;
mod punch;
pub use mapping::{
    AnswerVoice, GATEWAY_RECHECK_INTERVAL, INTERNET_POLL_INTERVAL, InternetSwitch, InternetWatch,
    KeeperHandle, KeeperStep, MAPPING_LIFETIME_SECS, MAPPING_RENEW_INTERVAL, MAPPING_RETRY_LADDER,
    MAPPING_STOP_POLL, MappedOver, MappingAnswer, MappingBoot, MappingBootCounts, MappingGuard,
    MappingKeeper, MappingRetry, MappingStep, external_address_report, external_socket,
    internet_is_on, keep_internet_mapping, keeper_step, mapping_answer_voice, mapping_boot,
    mapping_boot_counts, record_mappings_at, recorded_external_socket, run_mapping,
    run_mapping_while, run_mapping_with_upnp, set_internet, set_internet_via, spawn_mapping_keeper,
    start_peer_mapping, upnp_discoverer,
};
pub use observed::{
    forget_observations, observed_peer_for, observed_self_addresses, observed_self_for,
    remember_observed_peer, remember_observed_self,
};
pub use port::{
    PORT_CEILING, PORT_FLOOR, SLOT_SECONDS, accepted_ports, current_slot, derived_port,
    forget_port_secret, port_is_accepted, port_secret, port_secret_for, remember_pair_hash,
    remember_port_secret, rendezvous_ports, restore_from_peers,
};
pub use punch::{
    KernelPunchNet, PUNCH_RETRY, PUNCH_SLOTS, PunchArrival, PunchFailure, PunchNet, PunchSlot,
    Punched, aimable_address, learn_from_hint, own_collapse_hint, punch, punch_is_possible,
    punch_plan, punch_plan_from_slot, punch_request, punch_slots_within, punch_target,
    reach_no_mapping_line, reach_punch_line, self_address_for, slot_window,
};
