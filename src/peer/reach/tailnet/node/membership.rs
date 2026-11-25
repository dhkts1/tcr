//! Join and leave, the CLI's half of node mode.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};

use super::device::{Authorization, DeviceFactory, NodeSelf, TailnetDevice, bounded};
use super::keys::{
    AuthKey, NodeKeyFile, check_node_name, read_key_file, remove_key_file, write_key_file,
};
use super::{CONTROL_TIMEOUT, SHUTDOWN_DEADLINE};
use crate::peer::config::TailnetMode;
use crate::peer::state::{TailnetRecord, TailnetState};

// ---------------------------------------------------------------------------
// Join and leave, the CLI's half
// ---------------------------------------------------------------------------

/// `tcr peer tailnet node --auth-key -`: join the tailnet, then set the mode.
///
/// The order is the contract. A device is built in this process with the
/// auth key; control has to say the node is authorized within `deadline`; the
/// device is shut down; the key file is written (0600, never over an existing
/// one); and only then is `tailnet: "node"` written, so a server never sees
/// the mode before the keys it needs exist. A refused key leaves both files as
/// they were.
///
/// Refuses while the mode is already `node` with a key file: the serving
/// process may be running a device with those keys right now, and two
/// devices with one node key fight over it. `leave` first.
pub async fn join_with<F: DeviceFactory>(
    factory: &F,
    peers_path: &Path,
    key_path: &Path,
    name: Option<String>,
    auth_key: AuthKey,
    deadline: Duration,
) -> Result<NodeSelf> {
    if !F::SUPPORTED {
        bail!(
            "peer tailnet: this tcr was built without tcr's own tailnet node, so it cannot join \
             one; build tcr from source with `--features tailnet-node`, or use the Tailscale app"
        );
    }
    if let Some(name) = &name {
        check_node_name(name)?;
    }
    let file = crate::peer::config::read_or_default(peers_path)?;
    let existing = read_key_file(key_path)?;
    if file.tailnet_mode() == TailnetMode::Node && existing.is_some() {
        bail!(
            "peer tailnet: this Mac is already a node on a tailnet; run `tcr peer tailnet leave` \
             first to join again"
        );
    }
    let fresh = existing.is_none();
    let keys = match existing {
        Some(keys) => {
            if name.is_some() && name != keys.hostname {
                bail!(
                    "peer tailnet: {} already holds keys under another name; run `tcr peer \
                     tailnet leave` first to join under a new one",
                    key_path.display()
                );
            }
            keys
        }
        None => NodeKeyFile {
            key_state: factory.mint_keys()?,
            hostname: name,
        },
    };

    let device = bounded(
        deadline,
        "joining the tailnet",
        factory.build(&keys, Some(auth_key)),
    )
    .await?;
    let joined = async {
        // A refused key does not come back as `NotAuthorized` at 0.6.1: the
        // library's control task exits and the question fails with its
        // "actor missing" error (upstream #383; seen here with a made-up
        // key). So a failure is worded as both things it can be.
        let answer = bounded(deadline, "control", device.authorization())
            .await
            .context(
                "peer tailnet: the tailnet did not accept this auth key, or control could not \
                 be reached; check the key in the Tailscale admin console and the network",
            )?;
        match answer {
            Authorization::Authorized => {}
            Authorization::NotAuthorized => bail!(
                "peer tailnet: the tailnet did not accept this auth key; make a new one in the \
                 Tailscale admin console and try again"
            ),
        }
        bounded(CONTROL_TIMEOUT, "the node's address", device.self_node()).await
    }
    .await;
    // The device is shut down on every path: a node left running in a CLI
    // process that is about to exit is a node control sees vanish.
    let me = match joined {
        Ok(me) => me,
        Err(err) => {
            device.shutdown(SHUTDOWN_DEADLINE).await;
            return Err(err);
        }
    };
    // Shut down BEFORE the key file exists: a server already in node mode
    // starts its own device from that file on its next wake, and two devices
    // on one key fight over it.
    if !device.shutdown(SHUTDOWN_DEADLINE).await {
        tracing::warn!("peer tailnet: the joining node was stopped by force");
    }
    if fresh && let Err(err) = write_key_file(key_path, &keys) {
        return Err(err.context(
            "peer tailnet: the tailnet accepted this node but its keys could not be saved; \
             remove the machine in the Tailscale admin console and join again",
        ));
    }

    crate::peer::config::PeerFile::save_tailnet(peers_path, TailnetMode::Node)?;
    Ok(me)
}

/// What `leave` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaveOutcome {
    /// The key file was deleted.
    Deleted,
    /// There was no key file; only the mode was set.
    NoKeyFile,
}

/// `tcr peer tailnet leave`: set the mode to `off`, wait until the serving
/// process has released the node, then delete the key file.
///
/// The wait is what stops the delete pulling the keys out from under a node
/// the server is still running. It waits for `nodeReleased` written after
/// this call set the mode: the one record only the node's supervisor writes,
/// and only once no device built from the keys is alive. `mode == off` is not
/// enough, because the browse tick writes `off` records too, whatever the node
/// is doing.
///
/// It is skipped when no fresh node-mode record exists when this starts
/// (`held` absent, stale, or not in node mode): then no server holds a node.
/// When a server holds one and has not released it within `wait`, the key file
/// is kept and this fails, saying so.
pub async fn leave_with(
    peers_path: &Path,
    key_path: &Path,
    wait: Duration,
    poll: Duration,
) -> Result<LeaveOutcome> {
    let state_path = crate::peer::state::peer_state_path(peers_path);
    let held_at = |now_ms: i64| -> Result<Option<TailnetRecord>> {
        Ok(crate::peer::state::load(&state_path, now_ms)
            .with_context(|| format!("reading {}", state_path.display()))?
            .tailnet)
    };

    let asked_ms = crate::now_ms();
    let holds_a_node = held_at(asked_ms)?.is_some_and(|record| {
        record.mode.effective() == TailnetMode::Node && !record.is_stale(asked_ms)
    });
    crate::peer::config::PeerFile::save_tailnet(peers_path, TailnetMode::Off)?;

    let until = tokio::time::Instant::now() + wait;
    if holds_a_node {
        loop {
            let released = held_at(crate::now_ms())?.is_some_and(|record| {
                record.state == TailnetState::NodeReleased && record.recorded_at_ms >= asked_ms
            });
            if released {
                break;
            }
            if tokio::time::Instant::now() >= until {
                bail!(
                    "peer tailnet leave: the mode is now off, but the running tcr has not released \
                 the node after {}s, so {} was kept; run this again in a moment",
                    wait.as_secs(),
                    key_path.display()
                );
            }
            tokio::time::sleep(poll).await;
        }
    }

    if remove_key_file(key_path)? {
        Ok(LeaveOutcome::Deleted)
    } else {
        Ok(LeaveOutcome::NoKeyFile)
    }
}

/// The sentence `leave` ends with. The library has no call that logs a node
/// out of control (its `Device` has none at 0.6.1), so the machine stays in
/// the admin console until it is removed there.
pub const LEAVE_ADMIN_CONSOLE_NOTE: &str = "tcr cannot log a node out of the tailnet itself, so \
     this machine stays listed in the Tailscale admin console until you remove it there";
