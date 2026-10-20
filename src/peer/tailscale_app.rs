//! Reading the Tailscale app already running on this Mac.
//!
//! # The CLI, and not LocalAPI
//!
//! The three macOS installs of Tailscale (App Store, the standalone package,
//! and the open-source `tailscaled` from Homebrew) reach their LocalAPI three
//! different ways: a token in a file only the admin group can read, a token
//! hidden in a sandbox file name that only `lsof` finds, and a unix socket.
//! The CLI already handles all three, and `status` is the part Tailscale's own
//! client marks stable. So tcr runs `tailscale status --json` and reads its
//! output, under a deadline.
//!
//! # A fixed order, never `PATH`
//!
//! A Mac can hold two CLIs at different versions (the app's own binary and a
//! Homebrew one, with Homebrew first on `PATH`). The app's binary is the one
//! that talks to the app the operator sees, so [`CLI_PATHS`] puts it first.
//!
//! # Parsed once, into only what tcr reads
//!
//! The status document also carries every peer's owner login and DNS name.
//! None of that is read, so none of it is held: [`Status`] keeps the fields
//! tcr decides on and serde drops the rest unread. Nothing here logs a value.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Deserializer};

use crate::log_err::LogErr as _;
use crate::peer::state::TailnetState;

/// Where the Tailscale CLI is looked for, in order. The first that exists is
/// the one run.
///
/// 1. The app's own binary, which acts as the CLI when given arguments. The
///    App Store and standalone installs both put it here.
/// 2. The launcher the standalone app's "CLI integration" setting installs.
/// 3. Homebrew's `tailscale`, which talks to Homebrew's `tailscaled`.
pub const CLI_PATHS: [&str; 3] = [
    "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
    "/usr/local/bin/tailscale",
    "/opt/homebrew/bin/tailscale",
];

/// How long one `status --json` may take before it is killed. A healthy run
/// answers in well under a second; this bounds a wedged app, not a slow one.
pub const STATUS_TIMEOUT: Duration = Duration::from_secs(5);

/// [`CLI_PATHS`] as paths, the list production passes to [`read_status`].
pub fn default_cli_paths() -> Vec<PathBuf> {
    CLI_PATHS.iter().map(PathBuf::from).collect()
}

/// Why [`read_status`] has no status to give.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadError {
    /// No CLI at any of the paths searched.
    NotInstalled,
    /// The CLI ran but the app is not connected to its tailnet.
    NotRunning(NotRunning),
    /// The CLI was still running at the deadline. It was killed and reaped.
    TimedOut,
    /// The CLI exists but could not be started.
    SpawnFailed(std::io::ErrorKind),
    /// The CLI printed something that is not a status document.
    ParseFailed {
        /// Where in the output the parse stopped, 1-based.
        line: usize,
        /// The column on that line, 1-based.
        column: usize,
    },
}

/// The two ways the app can be "not running".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotRunning {
    /// The app answered with a backend state other than `Running`.
    Backend(BackendState),
    /// The CLI exited non-zero, which is what it does when it cannot reach
    /// the app at all ("is Tailscale running?"). The exit code, when there
    /// was one; a signal leaves none.
    CliExited(Option<i32>),
}

impl ReadError {
    /// The contract state a server records for this failure.
    pub const fn state(self) -> TailnetState {
        match self {
            Self::NotInstalled => TailnetState::AppMissing,
            Self::NotRunning(_) => TailnetState::AppNotRunning,
            Self::TimedOut | Self::SpawnFailed(_) | Self::ParseFailed { .. } => {
                TailnetState::ReadFailed
            }
        }
    }

    /// One plain sentence for the operator.
    pub fn detail(self) -> String {
        match self {
            Self::NotInstalled => "The Tailscale app is not installed on this Mac.".to_string(),
            Self::NotRunning(NotRunning::Backend(state)) => format!(
                "The Tailscale app is not connected to a tailnet (it says {}).",
                state.as_str()
            ),
            Self::NotRunning(NotRunning::CliExited(_)) => {
                "The Tailscale app is not running on this Mac.".to_string()
            }
            Self::TimedOut => format!(
                "The Tailscale app did not answer within {} seconds.",
                STATUS_TIMEOUT.as_secs()
            ),
            Self::SpawnFailed(kind) => format!("The Tailscale CLI could not be started ({kind})."),
            Self::ParseFailed { line, column } => format!(
                "The Tailscale CLI printed something tcr could not read (line {line}, column \
                 {column})."
            ),
        }
    }
}

/// `BackendState` in the status document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum BackendState {
    /// No state yet.
    NoState,
    /// Signed out.
    NeedsLogin,
    /// Signed in, waiting for an admin to approve the machine.
    NeedsMachineAuth,
    /// Turned off in the app.
    Stopped,
    /// Connecting.
    Starting,
    /// Connected. The only state [`read_status`] returns a status for.
    Running,
    /// A state this build does not know. Not a parse failure: a newer app
    /// adding a state must not read as a broken one.
    #[serde(other)]
    Other,
}

impl BackendState {
    /// The word the app uses for it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoState => "NoState",
            Self::NeedsLogin => "NeedsLogin",
            Self::NeedsMachineAuth => "NeedsMachineAuth",
            Self::Stopped => "Stopped",
            Self::Starting => "Starting",
            Self::Running => "Running",
            Self::Other => "an unknown state",
        }
    }
}

/// The operating system a peer reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Os {
    /// `macOS`.
    MacOs,
    /// The empty string, which is what a node with no OS reports. tcr's own
    /// node (feature B) shows up this way.
    Unreported,
    /// Anything else, kept as reported.
    Other(String),
}

impl<'de> Deserialize<'de> for Os {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Ok(match text.as_str() {
            "macOS" => Self::MacOs,
            "" => Self::Unreported,
            _ => Self::Other(text),
        })
    }
}

/// This Mac, as the app sees it: `Self` in the status document.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SelfNode {
    /// This Mac's tailnet addresses, IPv4 and IPv6.
    #[serde(rename = "TailscaleIPs", default, deserialize_with = "null_as_default")]
    pub tailscale_ips: Vec<IpAddr>,
    /// The Tailscale user this Mac is signed in as.
    #[serde(rename = "UserID")]
    pub user_id: i64,
}

/// One other machine on the tailnet.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Peer {
    /// The host name the machine reports. Not unique and not a DNS name.
    #[serde(rename = "HostName", default)]
    pub host_name: String,
    /// Its tailnet addresses, IPv4 and IPv6.
    #[serde(rename = "TailscaleIPs", default, deserialize_with = "null_as_default")]
    pub tailscale_ips: Vec<IpAddr>,
    /// The OS it reports.
    #[serde(rename = "OS", default = "unreported")]
    pub os: Os,
    /// The Tailscale user that owns it.
    #[serde(rename = "UserID", default)]
    pub user_id: i64,
    /// Whether it is connected to Tailscale's control plane. Not proof that
    /// it is reachable from here.
    #[serde(rename = "Online", default)]
    pub online: bool,
    /// ACL tags (`tag:...`). Empty for a machine a person owns.
    #[serde(rename = "Tags", default, deserialize_with = "null_as_default")]
    pub tags: Vec<String>,
    /// Present only because another tailnet shared it with this one.
    #[serde(rename = "ShareeNode", default)]
    pub sharee_node: bool,
    /// The direct `ip:port` packets go to, or `None` when there is none.
    #[serde(rename = "CurAddr", default, deserialize_with = "empty_as_none")]
    pub cur_addr: Option<String>,
    /// The relay region packets go through, or `None` when there is none.
    #[serde(rename = "Relay", default, deserialize_with = "empty_as_none")]
    pub relay: Option<String>,
}

/// The parts of `tailscale status --json` tcr reads, parsed once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// The app's backend state. Always [`BackendState::Running`] on a status
    /// [`read_status`] returned.
    pub backend_state: BackendState,
    /// This Mac, or `None` when the app reported no `Self`.
    pub self_node: Option<SelfNode>,
    /// Every other machine, in the document's key order (node public key).
    pub peers: Vec<Peer>,
}

/// The document's top level, before the peer map becomes a list.
#[derive(Deserialize)]
struct RawStatus {
    #[serde(rename = "BackendState")]
    backend_state: BackendState,
    #[serde(rename = "Self", default)]
    self_node: Option<SelfNode>,
    #[serde(rename = "Peer", default, deserialize_with = "null_as_default")]
    peer: BTreeMap<String, Peer>,
}

fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

fn empty_as_none<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    Ok(Option::<String>::deserialize(deserializer)?.filter(|text| !text.is_empty()))
}

const fn unreported() -> Os {
    Os::Unreported
}

impl Status {
    /// Parse a status document. The backend state is not checked here;
    /// [`read_status`] does that.
    pub fn parse(json: &[u8]) -> Result<Self, ReadError> {
        let raw: RawStatus =
            serde_json::from_slice(json).map_err(|err| ReadError::ParseFailed {
                line: err.line(),
                column: err.column(),
            })?;
        Ok(Self {
            backend_state: raw.backend_state,
            self_node: raw.self_node,
            peers: raw.peer.into_values().collect(),
        })
    }

    /// This Mac's tailnet IPv4, the address a beacon and an invite carry.
    pub fn own_ipv4(&self) -> Option<Ipv4Addr> {
        self.self_node
            .as_ref()?
            .tailscale_ips
            .iter()
            .find_map(|ip| match ip {
                IpAddr::V4(v4) => Some(*v4),
                IpAddr::V6(_) => None,
            })
    }
}

/// The operator's own other Macs: online, untagged, not shared in from
/// another tailnet, signed in as the same Tailscale user as this Mac, and
/// reporting macOS or no OS at all (tcr's own node reports none).
///
/// Empty when the status has no `Self`, because with no user to compare to,
/// no machine is provably the operator's.
pub fn own_macs(status: &Status) -> Vec<&Peer> {
    let Some(me) = status.self_node.as_ref() else {
        return Vec::new();
    };
    status
        .peers
        .iter()
        .filter(|peer| {
            peer.online
                && peer.tags.is_empty()
                && !peer.sharee_node
                && peer.user_id == me.user_id
                && matches!(peer.os, Os::MacOs | Os::Unreported)
        })
        .collect()
}

/// The peer holding `addr`, if any. An IPv4 address wearing an IPv6 shape
/// (`::ffff:100.64.0.5`, what a dual-stack socket reports) is matched as the
/// IPv4 address it is.
pub fn owner_of(status: &Status, addr: IpAddr) -> Option<&Peer> {
    let addr = addr.to_canonical();
    status
        .peers
        .iter()
        .find(|peer| peer.tailscale_ips.contains(&addr))
}

/// How packets reach one peer, as the app reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// A direct path (`CurAddr` is set).
    Direct,
    /// Through a Tailscale relay (`Relay` is set, `CurAddr` is not).
    Relayed,
    /// Neither is reported.
    Unknown,
}

/// [`Route`] for `peer`.
pub const fn route(peer: &Peer) -> Route {
    match (&peer.cur_addr, &peer.relay) {
        (Some(_), _) => Route::Direct,
        (None, Some(_)) => Route::Relayed,
        (None, None) => Route::Unknown,
    }
}

/// Run the first CLI in `cli_paths` that exists as `status --json`, wait at
/// most `timeout`, and parse what it prints.
///
/// Blocks the calling thread for up to `timeout`; an async caller runs it on
/// a blocking thread. Returns a status only when the app is
/// [`BackendState::Running`].
///
/// The child's stdout is drained on its own thread while this one watches the
/// clock. Reading it only after the child exits would deadlock on a large
/// tailnet: the document passes the 64 KiB a pipe holds at a few dozen
/// peers, and a child blocked writing to a full pipe never exits.
pub fn read_status(cli_paths: &[PathBuf], timeout: Duration) -> Result<Status, ReadError> {
    let cli = find_cli(cli_paths).ok_or(ReadError::NotInstalled)?;
    let output = run_with_deadline(cli, timeout)?;
    let status = Status::parse(&output)?;
    if status.backend_state != BackendState::Running {
        return Err(ReadError::NotRunning(NotRunning::Backend(
            status.backend_state,
        )));
    }
    Ok(status)
}

/// The first path in `cli_paths` that is a file.
pub fn find_cli(cli_paths: &[PathBuf]) -> Option<&Path> {
    cli_paths
        .iter()
        .map(PathBuf::as_path)
        .find(|path| path.is_file())
}

/// `cli status --json`, its stdout, within `timeout`.
fn run_with_deadline(cli: &Path, timeout: Duration) -> Result<Vec<u8>, ReadError> {
    let started = Instant::now();
    let mut child = Command::new(cli)
        .args(["status", "--json"])
        // The app's binary starts its GUI when it thinks it was not run as a
        // CLI; this tells it it was. Tailscale's own advice for scripts.
        .env("TAILSCALE_BE_CLI", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| ReadError::SpawnFailed(err.kind()))?;
    let Some(mut stdout) = child.stdout.take() else {
        child
            .kill()
            .log_debug("killing a tailscale CLI with no stdout");
        child
            .wait()
            .log_debug("reaping a tailscale CLI with no stdout");
        return Err(ReadError::SpawnFailed(std::io::ErrorKind::BrokenPipe));
    };
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let read = stdout.read_to_end(&mut bytes).map(|_| bytes);
        // The receiver is gone only when the deadline already passed, and
        // then nobody wants these bytes.
        sender
            .send(read)
            .log_debug("handing the tailscale CLI's output back");
    });

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < timeout => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => {
                child
                    .kill()
                    .log_debug("killing a tailscale CLI past its deadline");
                child
                    .wait()
                    .log_debug("reaping a tailscale CLI past its deadline");
                return Err(ReadError::TimedOut);
            }
            Err(err) => {
                child
                    .kill()
                    .log_debug("killing a tailscale CLI that cannot be waited on");
                child
                    .wait()
                    .log_debug("reaping a tailscale CLI that cannot be waited on");
                return Err(ReadError::SpawnFailed(err.kind()));
            }
        }
    };
    // The child is gone, but something it started may still hold the pipe;
    // the remaining time bounds that wait too.
    let left = timeout.saturating_sub(started.elapsed());
    let bytes = match receiver.recv_timeout(left) {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(err)) => return Err(ReadError::SpawnFailed(err.kind())),
        Err(_) => return Err(ReadError::TimedOut),
    };
    if !status.success() {
        return Err(ReadError::NotRunning(NotRunning::CliExited(status.code())));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    /// A status document with fake values only: `Self` is user 111 at
    /// `100.64.0.1`, and each peer exercises one [`own_macs`] rule.
    const FIXTURE: &str = r#"{
      "Version": "1.0.0-fake",
      "BackendState": "Running",
      "TailscaleIPs": ["100.64.0.1", "fd7a:115c:a1e0::1"],
      "Self": {
        "HostName": "this-mac", "OS": "macOS", "UserID": 111, "Online": true,
        "TailscaleIPs": ["100.64.0.1", "fd7a:115c:a1e0::1"], "Tags": null,
        "CurAddr": "", "Relay": "fra"
      },
      "User": {"111": {"LoginName": "alice@example.com"}},
      "Peer": {
        "nodekey:01": {
          "HostName": "laptop-2", "OS": "macOS", "UserID": 111, "Online": true,
          "TailscaleIPs": ["100.64.0.2", "fd7a:115c:a1e0::2"], "Tags": null,
          "CurAddr": "192.0.2.10:41641", "Relay": "fra", "Unknown": [1, 2]
        },
        "nodekey:02": {
          "HostName": "tcr-studio", "OS": "", "UserID": 111, "Online": true,
          "TailscaleIPs": ["100.64.0.3"], "Tags": null, "CurAddr": "", "Relay": "fra"
        },
        "nodekey:03": {
          "HostName": "asleep", "OS": "macOS", "UserID": 111, "Online": false,
          "TailscaleIPs": ["100.64.0.4"], "Tags": null, "CurAddr": "", "Relay": ""
        },
        "nodekey:04": {
          "HostName": "coworker", "OS": "macOS", "UserID": 222, "Online": true,
          "TailscaleIPs": ["100.64.0.5"], "Tags": null, "CurAddr": "", "Relay": "fra"
        },
        "nodekey:05": {
          "HostName": "server", "OS": "macOS", "UserID": 111, "Online": true,
          "TailscaleIPs": ["100.64.0.6"], "Tags": ["tag:tcr"], "CurAddr": "", "Relay": ""
        },
        "nodekey:06": {
          "HostName": "shared-in", "OS": "macOS", "UserID": 111, "Online": true,
          "TailscaleIPs": ["100.64.0.7"], "Tags": null, "ShareeNode": true,
          "CurAddr": "", "Relay": ""
        },
        "nodekey:07": {
          "HostName": "phone", "OS": "iOS", "UserID": 111, "Online": true,
          "TailscaleIPs": ["100.64.0.8"], "Tags": null, "CurAddr": "", "Relay": "fra"
        }
      }
    }"#;

    fn fixture() -> Status {
        Status::parse(FIXTURE.as_bytes()).expect("the fixture parses")
    }

    fn names(peers: &[&Peer]) -> Vec<String> {
        peers.iter().map(|peer| peer.host_name.clone()).collect()
    }

    /// **The fixture parses into the fields tcr reads, and an unknown field
    /// is ignored.**
    #[test]
    fn a_status_document_parses_into_only_what_tcr_reads() {
        let status = fixture();
        assert_eq!(status.backend_state, BackendState::Running);
        let me = status.self_node.as_ref().expect("Self is present");
        assert_eq!(me.user_id, 111);
        assert_eq!(status.own_ipv4(), Some(Ipv4Addr::new(100, 64, 0, 1)));
        assert_eq!(status.peers.len(), 7);
        let laptop = &status.peers[0];
        assert_eq!(laptop.host_name, "laptop-2");
        assert_eq!(laptop.os, Os::MacOs);
        assert_eq!(laptop.cur_addr.as_deref(), Some("192.0.2.10:41641"));
        assert_eq!(status.peers[1].os, Os::Unreported);
        assert_eq!(status.peers[6].os, Os::Other("iOS".to_string()));
    }

    /// **`own_macs` keeps exactly the online, untagged, unshared Macs (or
    /// OS-less nodes) signed in as this Mac's user**, one rule per peer in the
    /// fixture.
    ///
    /// Watch it fail by dropping any one clause of the filter: the peer that
    /// clause exists for (`asleep`, `coworker`, `server`, `shared-in`,
    /// `phone`) joins the list.
    #[test]
    fn own_macs_applies_every_rule() {
        let status = fixture();
        assert_eq!(
            names(&own_macs(&status)),
            ["laptop-2", "tcr-studio"],
            "online, untagged, not shared in, same user, macOS or no OS"
        );
    }

    /// **With no `Self` there is no user to compare to, so nobody is ours.**
    #[test]
    fn own_macs_is_empty_without_self() {
        let mut status = fixture();
        status.self_node = None;
        assert!(own_macs(&status).is_empty());
    }

    /// **`owner_of` finds the peer by any of its addresses, and a v4-mapped
    /// form as its IPv4 self.**
    #[test]
    fn owner_of_finds_the_peer_holding_an_address() {
        let status = fixture();
        let by_v4 = owner_of(&status, "100.64.0.2".parse().expect("literal"));
        assert_eq!(by_v4.map(|p| p.host_name.as_str()), Some("laptop-2"));
        let by_v6 = owner_of(&status, "fd7a:115c:a1e0::2".parse().expect("literal"));
        assert_eq!(by_v6.map(|p| p.host_name.as_str()), Some("laptop-2"));
        let mapped = owner_of(&status, "::ffff:100.64.0.5".parse().expect("literal"));
        assert_eq!(mapped.map(|p| p.host_name.as_str()), Some("coworker"));
        assert!(owner_of(&status, "100.64.0.99".parse().expect("literal")).is_none());
        assert!(
            owner_of(&status, "100.64.0.1".parse().expect("literal")).is_none(),
            "this Mac's own address is not a peer's"
        );
    }

    /// **`route` reads `CurAddr` first, then `Relay`.**
    #[test]
    fn route_is_direct_then_relayed_then_unknown() {
        let status = fixture();
        assert_eq!(route(&status.peers[0]), Route::Direct, "CurAddr and Relay");
        assert_eq!(route(&status.peers[1]), Route::Relayed, "Relay only");
        assert_eq!(route(&status.peers[2]), Route::Unknown, "neither");
    }

    /// **`Peer: null`, `Self` absent and an unknown backend state all parse.**
    /// A logged-out app prints the first two; a newer app may print the third.
    #[test]
    fn a_sparse_document_parses() {
        let status = Status::parse(br#"{"BackendState": "SomethingNew", "Peer": null}"#)
            .expect("a sparse document parses");
        assert_eq!(status.backend_state, BackendState::Other);
        assert!(status.self_node.is_none());
        assert!(status.peers.is_empty());
        assert_eq!(status.own_ipv4(), None);
    }

    /// A fake CLI at `dir/name` running `body` under `/bin/sh`.
    fn fake_cli(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write the fake CLI");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("make the fake CLI executable");
        path
    }

    /// **The fake CLI is run with `status --json` and its document comes
    /// back.** The positive control for the failure cases below.
    #[test]
    fn a_running_app_answers_with_its_status() {
        let dir = tempfile::tempdir().expect("tempdir");
        let doc = dir.path().join("status.json");
        std::fs::write(&doc, FIXTURE).expect("write the fixture");
        let cli = fake_cli(
            dir.path(),
            "tailscale",
            &format!(
                "[ \"$1 $2\" = \"status --json\" ] || exit 9\ncat '{}'",
                doc.display()
            ),
        );
        let status = read_status(&[cli], STATUS_TIMEOUT).expect("the fake app answers");
        assert_eq!(names(&own_macs(&status)), ["laptop-2", "tcr-studio"]);
    }

    /// **The first path that exists wins, whatever comes after it.**
    #[test]
    fn the_first_cli_that_exists_is_the_one_run() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("not-here");
        let first = fake_cli(dir.path(), "first", r#"echo '{"BackendState": "Stopped"}'"#);
        let second = fake_cli(dir.path(), "second", "exit 1");
        assert_eq!(
            read_status(&[missing, first, second], STATUS_TIMEOUT),
            Err(ReadError::NotRunning(NotRunning::Backend(
                BackendState::Stopped
            ))),
            "the first existing path answered Stopped; the second would have exited 1"
        );
    }

    /// **A document larger than a pipe holds still comes back.** Read only
    /// after the child exits, this times out: the child blocks on a full
    /// pipe and never exits.
    ///
    /// Watch it fail by moving the stdout read after the `try_wait` loop.
    #[test]
    fn a_document_larger_than_a_pipe_is_read_in_full() {
        let dir = tempfile::tempdir().expect("tempdir");
        let doc = dir.path().join("status.json");
        let mut peers = String::new();
        for n in 0..2_000_u32 {
            if n > 0 {
                peers.push(',');
            }
            peers.push_str(&format!(
                r#""nodekey:{n}": {{"HostName": "laptop-{n}", "OS": "macOS", "UserID": 111,
                "Online": true, "TailscaleIPs": ["100.64.{}.{}"], "Tags": null,
                "CurAddr": "", "Relay": "fra"}}"#,
                n / 250,
                n % 250 + 1
            ));
        }
        let body = format!(
            r#"{{"BackendState": "Running", "Self": {{"UserID": 111, "TailscaleIPs": ["100.64.9.1"]}}, "Peer": {{{peers}}}}}"#
        );
        assert!(body.len() > 128 * 1024, "the document must outgrow a pipe");
        std::fs::write(&doc, body).expect("write the document");
        let cli = fake_cli(dir.path(), "tailscale", &format!("cat '{}'", doc.display()));
        let status = read_status(&[cli], STATUS_TIMEOUT).expect("a big document is read");
        assert_eq!(own_macs(&status).len(), 2_000);
    }

    /// **No CLI anywhere is `NotInstalled`, and the state says so.**
    #[test]
    fn no_cli_at_any_path_is_not_installed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = read_status(
            &[dir.path().join("a"), dir.path().join("b")],
            STATUS_TIMEOUT,
        )
        .expect_err("nothing to run");
        assert_eq!(err, ReadError::NotInstalled);
        assert_eq!(err.state(), TailnetState::AppMissing);
    }

    /// **A CLI past its deadline is killed, and the read is `TimedOut`.**
    ///
    /// Watch it fail by removing the deadline arm of the wait loop: the read
    /// then waits out the whole sleep and answers a parse failure instead.
    #[test]
    fn a_cli_past_its_deadline_is_timed_out() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cli = fake_cli(dir.path(), "tailscale", "exec /bin/sleep 30");
        let started = Instant::now();
        let err = read_status(&[cli], Duration::from_millis(300)).expect_err("too slow");
        assert_eq!(err, ReadError::TimedOut);
        assert_eq!(err.state(), TailnetState::ReadFailed);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the deadline, not the sleep, ended it: {:?}",
            started.elapsed()
        );
    }

    /// **`BackendState: Stopped` is not running**, and so is a CLI that
    /// exits non-zero because it cannot reach the app.
    #[test]
    fn a_stopped_app_or_a_failing_cli_is_not_running() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stopped = fake_cli(
            dir.path(),
            "stopped",
            r#"echo '{"BackendState": "Stopped", "Self": null, "Peer": null}'"#,
        );
        let err = read_status(&[stopped], STATUS_TIMEOUT).expect_err("stopped");
        assert_eq!(
            err,
            ReadError::NotRunning(NotRunning::Backend(BackendState::Stopped))
        );
        assert_eq!(err.state(), TailnetState::AppNotRunning);

        let failing = fake_cli(dir.path(), "failing", "exit 1");
        let err = read_status(&[failing], STATUS_TIMEOUT).expect_err("unreachable app");
        assert_eq!(err, ReadError::NotRunning(NotRunning::CliExited(Some(1))));
        assert_eq!(err.state(), TailnetState::AppNotRunning);
    }

    /// **Garbage is a parse failure with a position**, never an empty list.
    #[test]
    fn garbage_output_is_a_parse_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cli = fake_cli(dir.path(), "tailscale", "echo 'not json at all'");
        let err = read_status(&[cli], STATUS_TIMEOUT).expect_err("garbage");
        assert!(
            matches!(err, ReadError::ParseFailed { line: 1, .. }),
            "{err:?}"
        );
        assert_eq!(err.state(), TailnetState::ReadFailed);
    }

    /// **A file that cannot be executed is `SpawnFailed`**, not "missing".
    #[test]
    fn a_cli_that_cannot_start_is_a_spawn_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cli = dir.path().join("tailscale");
        std::fs::write(&cli, "not a program").expect("write");
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        let err = read_status(&[cli], STATUS_TIMEOUT).expect_err("not executable");
        assert!(matches!(err, ReadError::SpawnFailed(_)), "{err:?}");
        assert_eq!(err.state(), TailnetState::ReadFailed);
    }
}
