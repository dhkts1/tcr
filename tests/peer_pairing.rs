//! Two-phase pairing, the caps that make a knock cheap to
//! refuse, the network key, and the share link.
//!
//! # Everything here runs on this box, against temp files
//!
//! Every listener binds `127.0.0.1:0` (the kernel picks the port), every peers
//! file and state file is a fresh temp file at 0600, and nothing reads the
//! operator's config directory or cache directory. **Nothing here touches the proxy on
//! `127.0.0.1:3456`**: no test in this file connects to a port it did not bind
//! itself, and none starts, stops or signals anything.
//!
//! # The shape of the thing being tested
//!
//! Pairing splits in two. Phase one is a KNOCK: a `Noise_NN`
//! session with ephemeral keys on both sides, carrying `{instance_id,
//! proposed_name, wire_version}`, answered with one ack byte and closed. It
//! reveals no static key, grants nothing, and all it produces is a row an
//! operator reads. Phase two is `Noise_XX`, and it is answered **only** from an
//! instance id the operator accepted, at the address it knocked from, inside a
//! 120-second window. Everything else gets zero bytes.
//!
//! What that replaced: a node-wide two-minute window `tcr peer pair` opened on
//! the DIALLING Mac, during which any host that could reach the port got
//! message 2 and this node's static key with it.

use std::io::Write as _;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt as _;
use std::sync::Arc;
use std::time::Duration;

use tcr::log_err::LogErr as _;
use tcr::peer::config::join_key;
use tcr::peer::config::{self, NetworkKey, PeerFile};
use tcr::peer::id::NodeKey;
use tcr::peer::listener::{self, SessionContext};
use tcr::peer::noise::{self, Handshake};
use tcr::peer::pair;
use tcr::peer::state::{self, BanReason, PeerState};
use tcr_peer_wire::{INSTANCE_ID_BYTES, InstanceId, Knock, PeerId};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

#[path = "peer_pairing/caps.rs"]
mod caps;
#[path = "peer_pairing/cli_refusals.rs"]
mod cli_refusals;
#[path = "peer_pairing/endpoints.rs"]
mod endpoints;
#[path = "peer_pairing/enrolment.rs"]
mod enrolment;
#[path = "peer_pairing/harness.rs"]
mod harness;
#[path = "peer_pairing/invite_addresses.rs"]
mod invite_addresses;
#[path = "peer_pairing/knock.rs"]
mod knock;
#[path = "peer_pairing/knock_port.rs"]
mod knock_port;
#[path = "peer_pairing/network_key_and_link.rs"]
mod network_key_and_link;
#[path = "peer_pairing/operator_verbs.rs"]
mod operator_verbs;
#[path = "peer_pairing/opt_in.rs"]
mod opt_in;
