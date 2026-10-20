//! Finding the operator's own Macs over the tailnet, in app mode.
//!
//! The serving process's browse task drives this every
//! [`crate::peer::discovery::BROWSE_INTERVAL`] (`spawn_browse` in
//! `src/server.rs`): it reads the Tailscale app
//! ([`crate::peer::tailscale_app`]), sends this Mac's beacon to each of the
//! operator's own Macs as one UDP datagram on the peer port, and folds the
//! beacons it heard since the last wake into the same found list the mDNS
//! scan fills. Nothing here writes a file; the browse task is the one writer.
//!
//! # Why a beacon and not a port probe
//!
//! A TCP connect proves only that something accepted it, because the
//! listener writes nothing to a stranger, and every probe costs the far Mac a
//! socket slot and a refusal line. A beacon carries the instance id, so a Mac
//! heard over both mDNS and the tailnet is recognisably one Mac. It carries
//! what the mDNS beacon carries and nothing more
//! ([`crate::peer::discovery::tailnet_beacon_datagram`]): no key, no node id.
//!
//! # Who is heard
//!
//! The receiving socket is bound to this Mac's own tailnet IPv4, never
//! `0.0.0.0`, so nothing new listens on the LAN or the internet. A datagram
//! whose source is not one of the operator's own Macs in the latest reading
//! ([`crate::peer::tailscale_app::own_macs`]) is dropped before it is parsed.
//! WireGuard drops a packet whose source address is not the sending peer's
//! own, so another tailnet machine cannot borrow one of those addresses.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};

use crate::peer::config::{Locator, PeerRow, TailnetMode};
use crate::peer::discovery::{self, Discovered};
use crate::peer::state::{TailnetPeer, TailnetRecord, TailnetRoute, TailnetState};
use crate::peer::tailscale_app::{self, ReadError, Route, Status};

/// The least time between two datagrams read from one source. One beacon per
/// browse interval is what a sender sends; this leaves room for a restart
/// and bounds what a misbehaving Mac of the operator's own can cost.
pub const MIN_BEACON_GAP_MS: i64 = 5_000;

/// The most beacons held between two wakes. Every source is one of the
/// operator's own Macs, so this is far above what a real tailnet sends.
pub const MAX_HEARD_PER_TICK: usize = 16;

/// The receive buffer for one datagram: one byte past the largest beacon
/// read, so an oversized datagram (which the socket truncates to this size)
/// is still seen as oversized and refused.
pub const RECEIVE_BUFFER_BYTES: usize = discovery::TAILNET_BEACON_MAX_BYTES + 1;

/// The operator's own Macs' tailnet IPv4 addresses in `status`: where the
/// beacon goes, and the only sources one is read from.
pub fn own_mac_addresses(status: &Status) -> Vec<Ipv4Addr> {
    tailscale_app::own_macs(status)
        .iter()
        .flat_map(|peer| peer.tailscale_ips.iter())
        .filter_map(|ip| match ip {
            IpAddr::V4(v4) => Some(*v4),
            IpAddr::V6(_) => None,
        })
        .collect()
}

/// What became of one datagram offered to a [`BeaconIntake`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intake {
    /// Read and held for the next wake.
    Heard,
    /// Its source is not one of the operator's own Macs right now.
    NotOwnMac,
    /// Its source sent one less than [`MIN_BEACON_GAP_MS`] ago.
    TooSoon,
    /// [`MAX_HEARD_PER_TICK`] beacons are already held.
    Full,
    /// Not a beacon this node will show: see
    /// [`discovery::tailnet_beacon_row`].
    NotABeacon,
}

/// Beacons heard between two wakes of the browse task, bounded in count and
/// in rate per source.
#[derive(Debug, Default)]
pub struct BeaconIntake {
    last_offered_ms: HashMap<IpAddr, i64>,
    heard: Vec<Discovered>,
}

impl BeaconIntake {
    /// An empty intake.
    pub fn new() -> Self {
        Self::default()
    }

    /// Offer one datagram from `from`. `own_macs` is the latest reading's
    /// [`own_mac_addresses`]; anything else is dropped before it is parsed.
    pub fn offer(
        &mut self,
        from: IpAddr,
        datagram: &[u8],
        own_macs: &[Ipv4Addr],
        network_key: Option<&crate::peer::config::NetworkKey>,
        now_ms: i64,
    ) -> Intake {
        let from = from.to_canonical();
        let IpAddr::V4(source) = from else {
            return Intake::NotOwnMac;
        };
        if !own_macs.contains(&source) {
            return Intake::NotOwnMac;
        }
        if self.heard.len() >= MAX_HEARD_PER_TICK {
            return Intake::Full;
        }
        // Counted from the last datagram OFFERED, read or not, so a source
        // sending garbage costs at most one parse per gap.
        if let Some(last) = self.last_offered_ms.get(&from)
            && now_ms.saturating_sub(*last) < MIN_BEACON_GAP_MS
        {
            return Intake::TooSoon;
        }
        self.last_offered_ms.insert(from, now_ms);
        match discovery::tailnet_beacon_row(datagram, from, network_key, now_ms.div_euclid(1_000)) {
            Some(row) => {
                self.heard.push(row);
                Intake::Heard
            }
            None => Intake::NotABeacon,
        }
    }

    /// Hand over what was heard since the last call, and forget the rate
    /// entries of sources that are no longer the operator's Macs, so the map
    /// never holds more than the current reading's addresses.
    pub fn take(&mut self, own_macs: &[Ipv4Addr]) -> Vec<Discovered> {
        self.last_offered_ms.retain(|addr, _| match addr {
            IpAddr::V4(v4) => own_macs.contains(v4),
            IpAddr::V6(_) => false,
        });
        std::mem::take(&mut self.heard)
    }

    /// Drop everything held, for a mode that stopped reading beacons.
    pub fn clear(&mut self) {
        self.last_offered_ms.clear();
        self.heard.clear();
    }
}

/// What one app-mode tick learned from reading the Tailscale app.
pub type Reading<'a> = Result<&'a Status, ReadError>;

/// The record an app-mode tick writes as `held`: `found` is how many of the
/// operator's Macs the found list holds from a tailnet beacon.
pub fn record(reading: Reading<'_>, found: usize, now_ms: i64) -> TailnetRecord {
    let base = |state| TailnetRecord {
        mode: TailnetMode::App.into(),
        state,
        recorded_at_ms: now_ms,
        ..TailnetRecord::default()
    };
    match reading {
        Ok(status) => TailnetRecord {
            address: status.own_ipv4(),
            found: u32::try_from(found).unwrap_or(u32::MAX),
            ..base(TailnetState::Ready)
        },
        Err(err) => TailnetRecord {
            detail: Some(err.detail()),
            ..base(err.state())
        },
    }
}

/// One tick's `held` write to the state file at `state_path`. Whether it
/// wrote.
///
/// Writes only when `mode` is app, the one mode this tick records, from
/// `reading`, that tick's read of the app. Off writes nothing, not even a record saying "off": the panel decides Off
/// from the setting alone and never reads a record for it, and a write every
/// 20 s on every Mac with a listener is a periodic fsync for nothing (the top
/// disk wait in the 2026-10-15 profile). A record left from before the mode
/// went off is harmless, because the setting wins, and it goes stale on its
/// own. Node writes nothing from here either: tcr's own node has its own
/// supervisor, and that is the one writer of `held` in node mode (the lead,
/// 2026-10-20).
pub fn write_held(
    state_path: &std::path::Path,
    mode: TailnetMode,
    reading: Option<Reading<'_>>,
    found: usize,
    peers: Vec<TailnetPeer>,
    now_ms: i64,
) -> anyhow::Result<bool> {
    let (TailnetMode::App, Some(reading)) = (mode, reading) else {
        return Ok(false);
    };
    crate::peer::state::save_tailnet(state_path, record(reading, found, now_ms), peers)?;
    Ok(true)
}

/// The machines in `status` holding an address that some row in `rows` has
/// as a direct endpoint, with how the app reaches each. Only those: the rest
/// of a shared tailnet is other people's machines and has no business in
/// this Mac's state file.
pub fn pinned_peers(status: &Status, rows: &[PeerRow]) -> Vec<TailnetPeer> {
    let hosts: Vec<IpAddr> = rows
        .iter()
        .flat_map(|row| row.endpoints.iter())
        .filter_map(|endpoint| match endpoint.locator {
            Locator::Direct { addr } => Some(addr.ip().to_canonical()),
            Locator::Via { .. } | Locator::Reverse { .. } => None,
        })
        .collect();
    status
        .peers
        .iter()
        .filter(|peer| peer.tailscale_ips.iter().any(|ip| hosts.contains(ip)))
        .map(|peer| TailnetPeer {
            addresses: peer.tailscale_ips.clone(),
            route: match tailscale_app::route(peer) {
                Route::Direct => Some(TailnetRoute::Direct),
                Route::Relayed => Some(TailnetRoute::Relayed),
                Route::Unknown => None,
            },
        })
        .collect()
}

/// The beacon socket: UDP on this Mac's own tailnet IPv4 and the peer port,
/// or nothing at all.
#[derive(Debug)]
pub struct BeaconSocket {
    port: u16,
    bound: Option<(Ipv4Addr, tokio::net::UdpSocket)>,
}

impl BeaconSocket {
    /// No socket yet, for the peer port `port`.
    pub const fn new(port: u16) -> Self {
        Self { port, bound: None }
    }

    /// The address it is bound to, if any.
    pub fn bound_to(&self) -> Option<Ipv4Addr> {
        self.bound.as_ref().map(|(addr, _)| *addr)
    }

    /// Be bound to `own`: kept when it already is, rebound when this Mac's
    /// tailnet address changed. On an error nothing is left bound.
    pub async fn bind_to(&mut self, own: Ipv4Addr) -> std::io::Result<()> {
        if self.bound_to() == Some(own) {
            return Ok(());
        }
        self.bound = None;
        let socket = tokio::net::UdpSocket::bind(SocketAddrV4::new(own, self.port)).await?;
        self.bound = Some((own, socket));
        Ok(())
    }

    /// Close it. Whether one was open.
    pub fn close(&mut self) -> bool {
        self.bound.take().is_some()
    }

    /// Wait for one datagram. Never finishes while nothing is bound, so a
    /// `select!` arm on it simply never fires.
    pub async fn recv(&self, buf: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
        match &self.bound {
            Some((_, socket)) => socket.recv_from(buf).await,
            None => std::future::pending().await,
        }
    }

    /// Send `datagram` to each address in `to` on the peer port. How many
    /// sends the kernel took; each refusal is logged at debug, because a Mac
    /// of the operator's that is asleep is the ordinary case.
    pub async fn send(&self, to: &[Ipv4Addr], datagram: &[u8]) -> usize {
        let Some((_, socket)) = &self.bound else {
            return 0;
        };
        let mut sent = 0;
        for addr in to {
            match socket
                .send_to(datagram, SocketAddrV4::new(*addr, self.port))
                .await
            {
                Ok(_) => sent += 1,
                Err(err) => tracing::debug!(
                    to = %addr,
                    error = %err,
                    "peer find: a tailnet beacon was not sent"
                ),
            }
        }
        sent
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peer::config::{Endpoint, EndpointSource};
    use crate::peer::discovery::FoundSource;
    use tcr_peer_wire::{INSTANCE_ID_BYTES, InstanceId};

    /// `Self` is user 111 at 100.64.0.1; laptop-2 (100.64.0.2) is the
    /// operator's, coworker (100.64.0.5) is another user's, server
    /// (100.64.0.6) is tagged, shared-in (100.64.0.7) is shared in.
    const STATUS: &str = r#"{
      "BackendState": "Running",
      "Self": {"UserID": 111, "TailscaleIPs": ["100.64.0.1", "fd7a:115c:a1e0::1"]},
      "Peer": {
        "nodekey:01": {"HostName": "laptop-2", "OS": "macOS", "UserID": 111, "Online": true,
          "TailscaleIPs": ["100.64.0.2", "fd7a:115c:a1e0::2"], "CurAddr": "192.0.2.10:41641"},
        "nodekey:04": {"HostName": "coworker", "OS": "macOS", "UserID": 222, "Online": true,
          "TailscaleIPs": ["100.64.0.5"], "Relay": "fra"},
        "nodekey:05": {"HostName": "server", "OS": "macOS", "UserID": 111, "Online": true,
          "TailscaleIPs": ["100.64.0.6"], "Tags": ["tag:tcr"]},
        "nodekey:06": {"HostName": "shared-in", "OS": "macOS", "UserID": 111, "Online": true,
          "TailscaleIPs": ["100.64.0.7"], "ShareeNode": true}
      }
    }"#;

    fn status() -> Status {
        Status::parse(STATUS.as_bytes()).expect("the fixture parses")
    }

    fn beacon(instance: u8, port: u16) -> Vec<u8> {
        let txt = discovery::beacon_txt(
            &InstanceId([instance; INSTANCE_ID_BYTES]),
            port,
            Some("laptop-2"),
            None,
            1_000,
        );
        discovery::tailnet_beacon_datagram(&txt, port)
    }

    fn ip(text: &str) -> IpAddr {
        text.parse().expect("a literal address parses")
    }

    /// **Only the operator's own Macs' IPv4 addresses are candidates.**
    #[test]
    fn own_mac_addresses_are_the_ipv4_of_own_macs() {
        assert_eq!(own_mac_addresses(&status()), [Ipv4Addr::new(100, 64, 0, 2)]);
    }

    /// **A beacon from one of the operator's own Macs is heard, with the
    /// datagram's source as its address and `tailnet` as its source; one from
    /// any other tailnet address is dropped before it is read.**
    ///
    /// Watch it fail by removing the `own_macs.contains` check in `offer`:
    /// the coworker's, the tagged node's and the shared-in node's beacons are
    /// then heard too.
    #[test]
    fn a_beacon_from_a_non_candidate_is_dropped() {
        let own = own_mac_addresses(&status());
        let mut intake = BeaconIntake::new();
        for stranger in [
            "100.64.0.5",
            "100.64.0.6",
            "100.64.0.7",
            "192.168.1.20",
            "fd7a:115c:a1e0::2",
        ] {
            assert_eq!(
                intake.offer(ip(stranger), &beacon(9, 7755), &own, None, 100_000),
                Intake::NotOwnMac,
                "{stranger} is not one of the operator's own Macs over IPv4"
            );
        }
        assert_eq!(
            intake.offer(
                ip("::ffff:100.64.0.2"),
                &beacon(9, 7755),
                &own,
                None,
                100_000
            ),
            Intake::Heard,
            "the positive control: laptop-2, even in v4-mapped form"
        );
        let heard = intake.take(&own);
        assert_eq!(heard.len(), 1);
        assert_eq!(heard[0].addrs, ["100.64.0.2"]);
        assert_eq!(heard[0].sources, [FoundSource::Tailnet]);
        assert_eq!(heard[0].port, 7755);
        assert_eq!(heard[0].name.as_deref(), Some("laptop-2"));
        assert!(
            intake.take(&own).is_empty(),
            "take hands each beacon over once"
        );
    }

    /// **One source is read at most once per [`MIN_BEACON_GAP_MS`], and the
    /// intake holds at most [`MAX_HEARD_PER_TICK`].**
    #[test]
    fn the_intake_is_bounded_in_rate_and_count() {
        let own = own_mac_addresses(&status());
        let mut intake = BeaconIntake::new();
        let laptop = ip("100.64.0.2");
        assert_eq!(
            intake.offer(laptop, &beacon(9, 7755), &own, None, 100_000),
            Intake::Heard
        );
        assert_eq!(
            intake.offer(
                laptop,
                &beacon(9, 7755),
                &own,
                None,
                100_000 + MIN_BEACON_GAP_MS - 1
            ),
            Intake::TooSoon
        );
        assert_eq!(
            intake.offer(laptop, b"garbage", &own, None, 100_000 + MIN_BEACON_GAP_MS),
            Intake::NotABeacon
        );
        assert_eq!(
            intake.offer(
                laptop,
                &beacon(9, 7755),
                &own,
                None,
                100_000 + MIN_BEACON_GAP_MS + 1
            ),
            Intake::TooSoon,
            "the garbage datagram started the gap too"
        );

        let many: Vec<Ipv4Addr> = (1..=40).map(|n| Ipv4Addr::new(100, 64, 1, n)).collect();
        let mut intake = BeaconIntake::new();
        let heard = many
            .iter()
            .filter(|addr| {
                intake.offer(IpAddr::V4(**addr), &beacon(9, 7755), &many, None, 1_000)
                    == Intake::Heard
            })
            .count();
        assert_eq!(heard, MAX_HEARD_PER_TICK);
    }

    /// **`held` for each state an app-mode tick can reach**: the app read,
    /// and each read failure. Off and node record nothing: see the next test.
    #[test]
    fn the_record_for_each_state() {
        let status = status();
        let ready = record(Ok(&status), 2, 5);
        assert_eq!(ready.mode, TailnetMode::App.into());
        assert_eq!(ready.recorded_at_ms, 5);
        assert_eq!(ready.state, TailnetState::Ready);
        assert_eq!(ready.address, Some(Ipv4Addr::new(100, 64, 0, 1)));
        assert_eq!(ready.found, 2);
        assert_eq!(ready.detail, None);

        for (err, state) in [
            (ReadError::NotInstalled, TailnetState::AppMissing),
            (
                ReadError::NotRunning(tailscale_app::NotRunning::CliExited(Some(1))),
                TailnetState::AppNotRunning,
            ),
            (ReadError::TimedOut, TailnetState::ReadFailed),
            (
                ReadError::ParseFailed { line: 1, column: 1 },
                TailnetState::ReadFailed,
            ),
        ] {
            let failed = record(Err(err), 2, 5);
            assert_eq!(failed.mode, TailnetMode::App.into());
            assert_eq!(failed.state, state, "{err:?}");
            assert_eq!(failed.detail, Some(err.detail()), "a plain sentence");
            assert_eq!((failed.found, failed.address), (0, None));
        }
    }

    /// **Modes off and node, three ticks each, write nothing to the state
    /// file; app writes on every tick.** Node writes nothing even when handed
    /// a reading: tcr's own node supervisor is the one writer there.
    ///
    /// Watch it fail by letting `write_held` write whenever `reading` is
    /// `Some`, whatever the mode: the node rows then create the file.
    #[test]
    fn modes_off_and_node_write_nothing_and_app_writes_every_tick() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("peer-state.json");
        let status = status();
        let mut now = 1_000;
        for (mode, reading) in [
            (TailnetMode::Off, None),
            (TailnetMode::Node, None),
            (TailnetMode::Node, Some(Ok(&status))),
        ] {
            for tick in 0..3 {
                now += 20_000;
                let wrote = write_held(&path, mode, reading, 0, Vec::new(), now)
                    .expect("a tick that records nothing does not fail");
                assert!(!wrote, "{mode:?} tick {tick} wrote");
                assert!(
                    !path.exists(),
                    "{mode:?} tick {tick} created the state file"
                );
            }
        }
        for tick in 0..3 {
            now += 20_000;
            let wrote = write_held(
                &path,
                TailnetMode::App,
                Some(Ok(&status)),
                1,
                Vec::new(),
                now,
            )
            .expect("an app tick writes");
            assert!(wrote, "app tick {tick} did not write");
            let state = crate::peer::state::load(&path, now).expect("the state file loads");
            let record = state.tailnet.expect("a record");
            assert_eq!(record.state, TailnetState::Ready);
            assert_eq!(
                record.recorded_at_ms, now,
                "app tick {tick} wrote this tick's record"
            );
        }
    }

    /// **Only machines a pinned row can be dialled at are recorded, with
    /// their route.**
    #[test]
    fn pinned_peers_records_only_pinned_machines() {
        let mut row = PeerRow {
            node: tcr_peer_wire::PeerId([1; 32]),
            label: "laptop-2".to_string(),
            endpoints: Vec::new(),
            added_at: 1,
            allow: crate::peer::config::Allow::default(),
            lend: Vec::new(),
            rendezvous_secret: None,
            sees_us_at: None,
        };
        row.endpoints = vec![
            Endpoint::direct(
                "100.64.0.2:7755".parse().expect("literal"),
                1,
                EndpointSource::Hello,
            ),
            Endpoint::direct(
                "192.168.1.20:7755".parse().expect("literal"),
                1,
                EndpointSource::Hello,
            ),
        ];
        let peers = pinned_peers(&status(), &[row]);
        assert_eq!(peers.len(), 1, "{peers:?}");
        assert_eq!(peers[0].route, Some(TailnetRoute::Direct));
        assert!(peers[0].addresses.contains(&ip("100.64.0.2")));
    }

    /// **The socket binds to the address it is given, never the wildcard,
    /// and a datagram sent to it arrives.**
    #[tokio::test]
    async fn the_socket_binds_the_given_address_and_receives() {
        let mut socket = BeaconSocket::new(0);
        socket
            .bind_to(Ipv4Addr::LOCALHOST)
            .await
            .expect("bind loopback");
        assert_eq!(socket.bound_to(), Some(Ipv4Addr::LOCALHOST));
        let local = match &socket.bound {
            Some((_, udp)) => udp.local_addr().expect("a bound socket has an address"),
            None => panic!("bound above"),
        };
        assert_eq!(local.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));

        let sender = std::net::UdpSocket::bind("127.0.0.1:0").expect("sender");
        sender
            .send_to(&beacon(9, 7755), local)
            .expect("send a beacon");
        let mut buf = [0_u8; RECEIVE_BUFFER_BYTES];
        let (n, from) =
            tokio::time::timeout(std::time::Duration::from_secs(5), socket.recv(&mut buf))
                .await
                .expect("a datagram within 5 s")
                .expect("recv");
        assert_eq!(&buf[..n], beacon(9, 7755).as_slice());
        assert_eq!(from.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert!(socket.close());
        assert_eq!(socket.bound_to(), None);
    }
}
