//! tcr's own tailnet node: one `tailscale::Device`, supervised, behind the
//! `tailnet-node` cargo feature.
//!
//! With the setting at `node` (`tcr peer tailnet node`), tcr joins a tailnet as
//! a machine of its own, through Tailscale's Rust library, so a Mac without the
//! Tailscale app can still be reached. Only tcr is on the tailnet, not the rest
//! of the Mac. Spec: `docs/design/tailnet.md` § B.
//!
//! # Source builds only
//!
//! The library supports Apple silicon and Linux, not Intel Macs, and brings a
//! C crypto library this repository otherwise avoids. Gil, 2026-10-20: source
//! builds only, behind the `tailnet-node` feature, which no release build
//! turns on. [`SUPPORTED`](crate::peer::tailnet_node::SUPPORTED) is the one place that decides whether this binary
//! can act on the setting; everything here that does not touch the library
//! itself compiles and is tested in every build, so CI exercises the
//! supervisor, the key file and the join and leave orderings without the
//! feature.
//!
//! # Who does what
//!
//! - **Joining is the CLI's** ([`join_with`](crate::peer::tailnet_node::join_with)). It reads the auth key from
//!   stdin, builds a device in its own process, waits until control says the
//!   node is authorized, writes the key file, shuts the device down, and only
//!   then sets the mode. The auth key is held in an [`AuthKey`](crate::peer::tailnet_node::AuthKey), which has no
//!   `Display` and a redacting `Debug`, and is never written anywhere.
//! - **Serving is the server's** ([`Supervisor`](crate::peer::tailnet_node::Supervisor)). It starts its device from
//!   the key file alone, listens on the node's address at the peer port, and
//!   hands every accepted connection to the same
//!   [`crate::peer::listener::serve_one`] the kernel listener uses. Admission
//!   needs no change: the source is a tailnet address and the bind is the
//!   node's, neither LAN nor loopback, so a stranger on the tailnet gets
//!   return visits and enrolments only (`internet_admission`).
//! - **Leaving is the CLI's** ([`leave_with`](crate::peer::tailnet_node::leave_with)): mode off, wait until the
//!   serving process has let go, then delete the key file.
//!
//! # Why a supervisor
//!
//! About a minute without network panics the library's relay and control
//! tasks, and nothing restarts them (upstream issue #439): the process lives
//! on with a dead tailnet, which is exactly what a laptop that sleeps does. So
//! the server checks the device every [`HEALTH_EVERY`](crate::peer::tailnet_node::HEALTH_EVERY) (a `self_node()`
//! lookup under [`CONTROL_TIMEOUT`](crate::peer::tailnet_node::CONTROL_TIMEOUT)) and whenever the default gateway changes,
//! and a failed check shuts the device down within [`SHUTDOWN_DEADLINE`](crate::peer::tailnet_node::SHUTDOWN_DEADLINE) and
//! builds a new one from the same keys.
//!
//! # Every wait on control is bounded
//!
//! Before registration, `ipv4_addr`, `self_node` and `is_authorized` wait
//! until control answers, which with no network is forever. Every such call
//! here runs under [`CONTROL_TIMEOUT`](crate::peer::tailnet_node::CONTROL_TIMEOUT), and nothing asks for an address before
//! [`TailnetDevice::authorization`](crate::peer::tailnet_node::TailnetDevice::authorization) has said yes.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, Result};
use serde::Serialize;

/// Whether this binary can act on `tailnet: "node"`: built with the
/// `tailnet-node` feature, for the one Mac target the library supports.
///
/// Read by the verb (`node` is refused without it), by `tcr peer tailnet
/// --json` (`nodeSupported`) and by the server's supervisor
/// (`nodeUnsupported`).
pub const SUPPORTED: bool = cfg!(all(
    feature = "tailnet-node",
    target_os = "macos",
    target_arch = "aarch64"
));

/// `tcr peer tailnet --json`: the setting, whether this binary can act on
/// `node`, and what the serving process last recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TailnetReport {
    /// The setting, as the peers file says now: the word as written, a word
    /// this build does not know included.
    pub mode: crate::peer::config::TailnetSetting,
    /// Whether this binary was built with the `tailnet-node` cargo feature.
    pub node_supported: bool,
    /// What the serving process recorded, or `None` when none has.
    /// Serialized as `null`, never left out: a reader tells "no server has
    /// recorded one" from a missing key by this.
    pub held: Option<crate::peer::state::TailnetRecord>,
}

impl TailnetReport {
    /// The report for the peers file at `peers_path` and the state file
    /// beside it.
    pub fn read(peers_path: &Path, now_ms: i64) -> Result<Self> {
        let file = crate::peer::config::read_or_default(peers_path)?;
        let state_path = crate::peer::state::peer_state_path(peers_path);
        let state = crate::peer::state::load(&state_path, now_ms)
            .with_context(|| format!("reading {}", state_path.display()))?;
        Ok(Self {
            mode: file.tailnet,
            node_supported: SUPPORTED,
            held: state.tailnet,
        })
    }
}

/// How often the supervisor wakes: re-reads the mode and refreshes the record.
pub const TICK: Duration = Duration::from_secs(10);

/// How often a running device is asked whether it is still alive.
pub const HEALTH_EVERY: Duration = Duration::from_secs(30);

/// The ceiling on any one wait on control: `is_authorized`, `self_node`,
/// `ipv4_addr`. Each waits forever without network.
pub const CONTROL_TIMEOUT: Duration = Duration::from_secs(10);

/// The ceiling on building a device (`Device::new` waits for its actors to
/// start).
pub const BUILD_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a device gets to stop gracefully before it is killed.
pub const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(5);

/// How long the first failed start waits before the next attempt. Each failure
/// in a row doubles it, up to [`RETRY_CAP`]; a device that passes a health
/// check puts it back here.
pub const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(30);

/// How long keys the tailnet refused wait before they are tried again: an
/// operator has to act (approve the machine, or join again), so asking control
/// every 30 s would be noise. The first rung for a refusal; it doubles to
/// [`RETRY_CAP`] like any other failure.
pub const RETRY_AFTER_REFUSAL: Duration = Duration::from_secs(300);

/// The longest a failed start waits before the next attempt: an offline
/// laptop asks control at most every ten minutes.
pub const RETRY_CAP: Duration = Duration::from_secs(600);

/// The wait after `failures` failures in a row, the first of them at `first`:
/// `first`, doubling, capped at [`RETRY_CAP`].
pub fn retry_wait(first: Duration, failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(16);
    first.saturating_mul(1_u32 << doublings).min(RETRY_CAP)
}

/// How long `tcr peer tailnet node --auth-key -` waits for control to accept
/// the key.
pub const JOIN_DEADLINE: Duration = Duration::from_secs(60);

/// How long `tcr peer tailnet leave` waits for the serving process to let go
/// of the node before it refuses to delete the key file.
pub const LEAVE_WAIT: Duration = Duration::from_secs(45);

/// How often `leave` re-reads the state file while it waits.
pub const LEAVE_POLL: Duration = Duration::from_millis(500);

/// A record whose content has not changed is still rewritten this often, so
/// its `recordedAtMs` stays inside the 60 s a reader calls fresh.
pub const RECORD_REFRESH: Duration = Duration::from_secs(20);

mod device;
mod dial;
mod keys;
#[cfg(all(feature = "tailnet-node", target_os = "macos", target_arch = "aarch64"))]
mod library;
#[cfg(all(
    test,
    feature = "tailnet-node",
    target_os = "macos",
    target_arch = "aarch64"
))]
mod library_tests;
mod live;
mod membership;
mod supervisor;
#[cfg(test)]
mod tests;

pub use device::{
    Accepted, Authorization, DeviceFactory, NoDevice, NoListener, NodeSelf, TailnetDevice,
    TailnetListener, Unsupported,
};
pub use dial::{
    DIAL_TIMEOUT, DialFn, DialSlot, Published, RouteDial, advertised_listen, allowed_dial_ports,
    default_peer_port, dial_for_cli, dial_for_route, dial_for_route_in, dial_in_slot,
    dial_through_node, dial_through_server, is_tailnet_ip, recorded_node_address, server_slot,
    splice_until_either_ends,
};
pub use keys::{
    AuthKey, KEY_FILE_NAME, NodeKeyFile, check_node_name, key_path, key_path_for_peers,
    read_key_file, remove_key_file, write_key_file,
};
pub use live::{LiveConnections, LiveGuard};
pub use membership::{LEAVE_ADMIN_CONSOLE_NOTE, LeaveOutcome, join_with, leave_with};
pub use supervisor::{GatewayFn, NodePaths, ServeFn, Supervisor};

/// The factory this binary builds devices with.
#[cfg(all(feature = "tailnet-node", target_os = "macos", target_arch = "aarch64"))]
pub(crate) type BuiltFactory = library::LibraryFactory;

/// The factory this binary builds devices with.
#[cfg(not(all(feature = "tailnet-node", target_os = "macos", target_arch = "aarch64")))]
pub(crate) type BuiltFactory = Unsupported;

/// `tcr peer tailnet node --auth-key -` with this binary's factory.
pub async fn join(peers_path: &Path, name: Option<String>, auth_key: AuthKey) -> Result<NodeSelf> {
    join_with(
        &BuiltFactory::default(),
        peers_path,
        &key_path_for_peers(peers_path),
        name,
        auth_key,
        JOIN_DEADLINE,
    )
    .await
}
