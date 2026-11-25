//! What this Mac can be reached on from off the LAN.
//!
//! Every test here runs against loopback or against a pure function. Nothing
//! reaches a real router, and nothing touches the running proxy: the NAT-PMP
//! tests stand a fake gateway on `127.0.0.1:0` and point the client at it, which
//! is the whole reason [`tcr::peer::reach::NatPmp::at`] takes an
//! address instead of always finding its own.

/// The environment every launch of the built `tcr` starts from.
#[path = "../../support/tcr_env.rs"]
mod tcr_env;

/// The harness pieces the `peer_*` test files share. Its capture layer keeps
/// each event's level, which is what the quiet-second-answer test reads.
#[path = "../../support/peer.rs"]
mod peer_support;

use peer_support::CaptureLayer;

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tcr::log_err::LogErr as _;

use tcr::peer::noise;
use tcr::peer::reach::{
    self, GATEWAY_PORT, MapProtocol, NatPmp, ReachError, ResultCode, TRIES, VERSION,
};

use std::path::Path;

use tcr::peer::config::{self as peer_config, PeerFile};
use tcr::peer::id::NodeKey;
use tcr::peer::listener::{self, InternetAdmission, SessionContext};
use tcr::peer::noise::Handshake;

/// NAT-PMP against a fake gateway: the three opcodes, refusals, the route table,
/// the IPv6 probe, a router with no NAT-PMP service and the retry ladder.
#[path = "peer_reach/nat_pmp.rs"]
mod nat_pmp;

/// The time-derived rendezvous port, what a dialler tries when the recorded port
/// stops answering, and the slots two Macs punch on.
#[path = "peer_reach/derived_port.rs"]
mod derived_port;

/// `tcr peer reach`, the mapping keeper and the `peer.internet` switch inside a
/// running server.
#[path = "peer_reach/keeper_and_switch.rs"]
mod keeper_and_switch;

/// The accept gate `peer.internet` turns on, the address the far side sees this
/// Mac at, and the home-LAN IPv6 case.
#[path = "peer_reach/accept_gate.rs"]
mod accept_gate;

/// A fake NAT-PMP gateway on loopback.
///
/// Answers the three opcodes the client sends and nothing else. `requests`
/// counts what arrived, which is what lets the retry test assert the RFC's
/// three sends rather than assert only that the call eventually failed.
struct FakeGateway {
    addr: SocketAddr,
    requests: Arc<AtomicU32>,
    /// One `(opcode, lifetime)` pair per request, in arrival order.
    ///
    /// The lifetime is what separates a map from a delete on the wire (RFC
    /// 6886 § 3.3 spells a delete as lifetime 0), and the order is the whole
    /// of item 1's gate: a keeper that renewed before it mapped, or never
    /// deleted, leaves a mapping table that looks the same either way.
    asked: Arc<Mutex<Vec<(u8, u32)>>>,
}

/// How the fake answers.
#[derive(Clone, Copy)]
enum Behaviour {
    /// Answer every opcode as a cooperative router would.
    Cooperative {
        /// The external port to hand back, which is deliberately NOT the
        /// internal port asked for: RFC 6886 § 3.3 allows a gateway to pick a
        /// different one, and a client that assumed its own number would
        /// advertise a port nothing listens behind.
        external_port: u16,
    },
    /// Answer every opcode with one result code.
    Refuse(u16),
    /// Read the request and never answer.
    Silent,
    /// Answer as a cooperative router would, but only after `after`.
    ///
    /// Every router is this one: a mapping is asked for at boot and granted a
    /// round trip later, and what reads the answer in between reads an absence.
    SlowCooperative {
        /// The external port to hand back, as [`Behaviour::Cooperative`].
        external_port: u16,
        /// How long this gateway thinks about it first.
        after: Duration,
    },
}

impl FakeGateway {
    /// Bind on loopback and serve `behaviour` until the process ends.
    fn start(behaviour: Behaviour) -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind the fake gateway on loopback");
        let addr = socket.local_addr().expect("the fake gateway's own address");
        let requests = Arc::new(AtomicU32::new(0));
        let counter = Arc::clone(&requests);
        let asked = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&asked);

        std::thread::spawn(move || {
            loop {
                let mut buffer = [0_u8; 32];
                let Ok((read, from)) = socket.recv_from(&mut buffer) else {
                    return;
                };
                counter.fetch_add(1, Ordering::SeqCst);
                let request = &buffer[..read];
                if let (Some(opcode), Some(lifetime)) =
                    (request.get(1).copied(), asked_lifetime(request))
                {
                    log.lock().expect("the fake's log").push((opcode, lifetime));
                }
                if let Some(response) = answer(request, behaviour) {
                    let _sent = socket.send_to(&response, from);
                }
            }
        });

        Self {
            addr,
            requests,
            asked,
        }
    }

    /// What this gateway was asked, in order.
    fn asked(&self) -> Vec<(u8, u32)> {
        self.asked.lock().expect("the fake's log").clone()
    }
}

/// The lifetime field of a mapping request, or 0 for an opcode that has none.
fn asked_lifetime(request: &[u8]) -> Option<u32> {
    if request.get(1).copied() == Some(0) {
        // Opcode 0 asks for the external address and carries no lifetime.
        return Some(0);
    }
    Some(u32::from_be_bytes([
        *request.get(8)?,
        *request.get(9)?,
        *request.get(10)?,
        *request.get(11)?,
    ]))
}

/// One response for one request, or [`None`] for the silent gateway.
fn answer(request: &[u8], behaviour: Behaviour) -> Option<Vec<u8>> {
    let opcode = *request.get(1)?;
    let (code, external_port) = match behaviour {
        Behaviour::Silent => return None,
        Behaviour::SlowCooperative {
            external_port,
            after,
        } => {
            std::thread::sleep(after);
            (0, external_port)
        }
        Behaviour::Refuse(code) => (code, 0),
        Behaviour::Cooperative { external_port } => (0, external_port),
    };

    let mut out = vec![VERSION, opcode | 0x80];
    out.extend_from_slice(&code.to_be_bytes());
    // The epoch counter, bytes 4..8 of every response.
    out.extend_from_slice(&7_777_u32.to_be_bytes());
    if opcode == 0 {
        out.extend_from_slice(&Ipv4Addr::new(203, 0, 113, 7).octets());
        return Some(out);
    }

    let internal_port = u16::from_be_bytes([*request.get(4)?, *request.get(5)?]);
    let asked_lifetime = u32::from_be_bytes([
        *request.get(8)?,
        *request.get(9)?,
        *request.get(10)?,
        *request.get(11)?,
    ]);
    // A delete (lifetime 0) is confirmed with zeroes, as RFC 6886 § 3.3
    // requires; anything else gets the port this fake chose.
    let (granted_external, granted_lifetime) = if asked_lifetime == 0 {
        (0, 0)
    } else {
        (external_port, asked_lifetime)
    };
    out.extend_from_slice(&internal_port.to_be_bytes());
    out.extend_from_slice(&granted_external.to_be_bytes());
    out.extend_from_slice(&granted_lifetime.to_be_bytes());
    Some(out)
}

/// A scratch directory of this test's own, so a sibling test binary running at
/// the same instant cannot read or write this one's peers file.
fn scratch(tag: &str) -> std::path::PathBuf {
    peer_support::scratch_dir("reach", tag)
}

/// A peers file with one pinned peer and a listener port, written as JSON text
/// rather than through `PeerFile`.
///
/// Text on purpose: this test is about the CLI's output, and going through the
/// config structs would make it fail to COMPILE every time a field is added
/// to those structs elsewhere, which is a different failure from the one this test exists to
/// report.
fn peers_file_with_one_peer(dir: &Path, listen_port: u16) -> std::path::PathBuf {
    let node = tcr_peer_wire::PeerId([0x22; 32]).to_wire();
    let body = format!(
        r#"{{
  "listen": "127.0.0.1:{listen_port}",
  "peers": [
    {{ "node": "{node}", "label": "studio-mac", "addedAt": 1 }}
  ]
}}"#
    );
    let path = dir.join("tcr-peers.json");
    std::fs::write(&path, body).expect("write the peers file");
    // The store refuses a file other accounts can read.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("0600 on the peers file");
    }
    path
}

/// The built `tcr` with its `HOME` the directory holding `file`, so no call reaches the
/// operator's files.
fn tcr_beside(file: &Path) -> std::process::Command {
    tcr_env::tcr(file.parent().expect("a fixture file has a directory"))
}
