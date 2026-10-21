//! CLI-to-CLI listener handoff. The Unix socket is private to the local user;
//! both ends check kernel peer credentials before exchanging a descriptor.
//! The signed-app handoff policy in `handoff` remains separate.
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

use crate::singleton::{self, ProxyKind};

const REQUEST: &[u8] = b"tcr-replace/2\n";
const RECEIPT: &[u8] = b"listener-received\n";
const RELEASED: &[u8] = b"listener-released\n";
const TIMEOUT: Duration = Duration::from_secs(15);
/// Total time the predecessor may spend handing off and draining. Minutes, not
/// seconds: a thinking response can stream for minutes, and the drain is what
/// lets it finish on the old process instead of being cut off.
pub const DRAIN_CEILING: Duration = Duration::from_secs(300);

/// Milliseconds that replace [`DRAIN_CEILING`] for one process. For tests only:
/// a test that holds a connection open forever would otherwise wait out the
/// full ceiling.
pub const DRAIN_CEILING_TEST_ENV: &str = "TCR_TEST_DRAIN_CEILING_MS";

/// The drain ceiling this process uses: [`DRAIN_CEILING`], unless
/// [`DRAIN_CEILING_TEST_ENV`] holds a whole number of milliseconds. A value that
/// does not parse is reported and ignored.
pub fn drain_ceiling() -> Duration {
    let Some(value) = std::env::var_os(DRAIN_CEILING_TEST_ENV) else {
        return DRAIN_CEILING;
    };
    match value.to_str().map(str::parse::<u64>) {
        Some(Ok(ms)) => Duration::from_millis(ms),
        _ => {
            tracing::warn!(
                value = %value.to_string_lossy(),
                "{DRAIN_CEILING_TEST_ENV} is not a whole number of milliseconds; using the default drain ceiling"
            );
            DRAIN_CEILING
        }
    }
}

fn current_uid() -> u32 {
    tcr_fdpass::effective_uid()
}

pub fn socket_path(pid: u32) -> PathBuf {
    // A short, per-uid path independent of HOME/XDG_CACHE_HOME also works when
    // launchd and a terminal disagree about their cache directories.
    PathBuf::from(format!("/tmp/tcr-restart-{}", current_uid())).join(format!("restart-{pid}.sock"))
}

/// A socketpair reports this process's effective credentials through the same
/// portable kernel API used for the peer. No environment-based user identity.
fn verify_same_user(peer: &UnixStream) -> io::Result<()> {
    let (ours, _other) = UnixStream::pair()?;
    if peer.peer_cred()?.uid() != ours.peer_cred()?.uid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "restart peer is a different user",
        ));
    }
    Ok(())
}

pub struct Endpoint {
    listener: UnixListener,
    path: PathBuf,
}

impl Endpoint {
    pub fn bind() -> io::Result<Self> {
        Self::bind_at(socket_path(std::process::id()))
    }

    fn bind_at(path: PathBuf) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            match std::fs::DirBuilder::new().mode(0o700).create(parent) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
            let meta = std::fs::symlink_metadata(parent)?;
            if !meta.is_dir() || meta.uid() != current_uid() || meta.mode() & 0o777 != 0o700 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "restart directory must be owned by this user with mode 0700",
                ));
            }
        }
        let listener = match UnixListener::bind(&path) {
            Ok(listener) => listener,
            Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
                // Only a refused connection proves this path is stale. Never
                // unlink a live endpoint or an inaccessible/unknown path.
                match std::os::unix::net::UnixStream::connect(&path) {
                    Err(probe) if probe.kind() == io::ErrorKind::ConnectionRefused => {
                        std::fs::remove_file(&path)?;
                        UnixListener::bind(&path)?
                    }
                    _ => return Err(error),
                }
            }
            Err(error) => return Err(error),
        };
        let endpoint = Self { listener, path };
        std::fs::set_permissions(&endpoint.path, std::fs::Permissions::from_mode(0o600))?;
        Ok(endpoint)
    }

    pub async fn accept(&self) -> std::os::unix::net::UnixStream {
        loop {
            let (mut peer, _) = match self.listener.accept().await {
                Ok(pair) => pair,
                Err(error) => {
                    tracing::warn!(%error, "restart accept failed; retrying");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            };
            let valid = async {
                verify_same_user(&peer)?;
                let mut request = [0; REQUEST.len()];
                peer.read_exact(&mut request).await?;
                if request != REQUEST {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "unknown restart protocol",
                    ));
                }
                let peer = peer.into_std()?;
                peer.set_nonblocking(false)?;
                peer.set_write_timeout(Some(TIMEOUT))?;
                Ok::<_, io::Error>(peer)
            };
            if let Ok(Ok(peer)) = tokio::time::timeout(TIMEOUT, valid).await {
                return peer;
            }
            tracing::warn!("refused invalid restart request");
        }
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        crate::log_err::LogErr::log_debug(
            std::fs::remove_file(&self.path),
            "removing restart endpoint",
        );
    }
}

/// Sending `SCM_RIGHTS` is not confirmation that recvmsg installed the descriptor.
/// Keep the sender's copy alive until the receiver explicitly acknowledges it.
pub(crate) async fn wait_for_listener_receipt(
    peer: &std::os::unix::net::UnixStream,
) -> io::Result<()> {
    let mut peer = peer.try_clone()?;
    peer.set_read_timeout(Some(TIMEOUT))?;
    tokio::task::spawn_blocking(move || {
        use std::io::Read as _;
        let mut receipt = [0; RECEIPT.len()];
        peer.read_exact(&mut receipt)?;
        if receipt != RECEIPT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid listener receipt",
            ));
        }
        Ok(())
    })
    .await
    .map_err(io::Error::other)?
}

/// Keep the handoff channel open until the sender has completed its send and
/// released the listener. The receiver waits for this before starting up.
pub(crate) fn confirm_listener_released(peer: &std::os::unix::net::UnixStream) -> io::Result<()> {
    use std::io::Write as _;
    let mut peer = peer;
    peer.write_all(RELEASED)
}

/// Request a socket only from a recognized standalone proxy on this port.
/// Failure leaves the incumbent alone: a kill-and-bind fallback would recreate
/// the refusal window this protocol exists to remove. Old binaries need one
/// explicit stop to upgrade; subsequent replacements use this handoff.
pub async fn request_listener(port: u16) -> anyhow::Result<Option<std::net::TcpListener>> {
    if port == 0 {
        return Ok(None);
    }
    let Some(incumbent) = singleton::live_proxy_server(port) else {
        return Ok(None);
    };
    if incumbent.kind == ProxyKind::TcrEmbedded {
        return Ok(None); // The existing stand-down policy protects the host app.
    }
    anyhow::ensure!(
        incumbent.kind == ProxyKind::Tcr,
        "the incumbent cannot hand over its listener; stop it explicitly once to upgrade; --replace left it running"
    );
    let mut peer = tokio::time::timeout(TIMEOUT, UnixStream::connect(socket_path(incumbent.pid)))
        .await.context("tcr-handoff-unavailable: timed out connecting to the incumbent handoff endpoint; stop it explicitly")?
        .context("tcr-handoff-unavailable: could not connect to the incumbent listener handoff endpoint; stop it explicitly, or use --replace if it runs this version or newer and handoff is enabled; no process was signalled")?;
    verify_same_user(&peer)?;
    tokio::time::timeout(TIMEOUT, peer.write_all(REQUEST))
        .await
        .context("timed out sending the listener handoff request")??;
    let peer = peer.into_std()?;
    peer.set_nonblocking(false)?;
    peer.set_read_timeout(Some(TIMEOUT))?;
    peer.set_write_timeout(Some(TIMEOUT))?;
    let listener = tokio::task::spawn_blocking(move || {
        use std::io::{Read as _, Write as _};
        let listener = tcr_fdpass::recv_listener(&peer)?;
        if listener.local_addr()? != std::net::SocketAddr::from(([127, 0, 0, 1], port)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "incumbent handed over a different listener",
            ));
        }
        let mut peer = peer;
        peer.write_all(RECEIPT)?;
        let mut released = [0; RELEASED.len()];
        peer.read_exact(&mut released)?;
        if released != RELEASED {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid listener release confirmation",
            ));
        }
        Ok(listener)
    })
    .await
    .context("restart receiver task failed")?
    .context("the incumbent did not hand over its listener; no process was signalled")?;
    Ok(Some(listener))
}

/// Disabled handoff must leave the ordinary server and its signal paths usable.
pub async fn accept(endpoint: Option<&Endpoint>) -> std::os::unix::net::UnixStream {
    match endpoint {
        Some(endpoint) => endpoint.accept().await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A thinking response can stream for minutes; a predecessor that stops
    /// draining after 30 s cuts it off mid-answer.
    #[test]
    fn the_drain_ceiling_outlasts_a_long_thinking_stream() {
        assert!(
            DRAIN_CEILING >= Duration::from_secs(300),
            "drain ceiling is {DRAIN_CEILING:?}"
        );
    }

    #[test]
    fn review_restart_faults_are_recoverable() {
        const CHILD: &str = "TCR_REVIEW_RESTART_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new("/bin/sh")
                .args(["-c", "ulimit -n 128; exec \"$@\"", "restart-fixture"])
                .arg(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "restart::tests::review_restart_faults_are_recoverable",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env("XDG_CACHE_HOME", "different-fixture-cache")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "restart accept must survive EMFILE and remain usable: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        assert_eq!(
            socket_path(42),
            PathBuf::from(format!(
                "/tmp/tcr-restart-{}/restart-42.sock",
                current_uid()
            )),
            "handoff discovery must not depend on successor cache variables"
        );
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                use std::io::Write;
                let root = tempfile::tempdir().unwrap();
                std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700))
                    .unwrap();
                let path = root.path().join("restart.sock");
                let endpoint = Endpoint::bind_at(path.clone()).unwrap();
                let mut peer = std::os::unix::net::UnixStream::connect(&path).unwrap();
                peer.write_all(REQUEST).unwrap();
                let mut files = Vec::new();
                loop {
                    match std::fs::File::open("/dev/null") {
                        Ok(file) => files.push(file),
                        Err(error) => {
                            assert_eq!(
                                error.raw_os_error(),
                                Some(24),
                                "must actually reach EMFILE"
                            );
                            break;
                        }
                    }
                }
                assert!(!files.is_empty(), "positive descriptor-pressure control");
                let accepting = endpoint.accept();
                tokio::pin!(accepting);
                assert!(
                    tokio::time::timeout(Duration::from_millis(150), &mut accepting)
                        .await
                        .is_err(),
                    "a transient accept error must stay pending while descriptors are exhausted"
                );
                drop(files);
                // The exhausted accept can close its peer; retry a fresh request.
                let mut retry = std::os::unix::net::UnixStream::connect(&path).unwrap();
                retry.write_all(REQUEST).unwrap();
                let accepted = tokio::time::timeout(Duration::from_secs(3), &mut accepting).await;
                assert!(
                    accepted.is_ok(),
                    "accept must resume after descriptor recovery"
                );
            });
    }

    #[tokio::test]
    async fn a_refused_stale_socket_can_be_rebound() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.path().join("restart.sock");
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        // Establish the named refusal precondition instead of assuming
        // listener release is already observable.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            match tokio::time::timeout(Duration::from_millis(100), UnixStream::connect(&path)).await
            {
                Ok(Err(error)) if error.kind() == io::ErrorKind::ConnectionRefused => break,
                Ok(Ok(peer)) => drop(peer),
                Ok(Err(error)) => panic!("unexpected stale-socket probe: {error}"),
                Err(_) => {}
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "socket never became refused"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let endpoint = Endpoint::bind_at(path.clone()).unwrap();
        assert!(UnixStream::connect(&path).await.is_ok());
        assert!(
            Endpoint::bind_at(path).is_err(),
            "must not steal a live endpoint"
        );
        drop(endpoint);
    }
}
