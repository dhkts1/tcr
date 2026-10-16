//! CLI-to-CLI listener handoff. The Unix socket is private to the local user;
//! both ends check kernel peer credentials before exchanging a descriptor.
//! The signed-app handoff policy in `handoff` remains separate.
use std::io;
use std::os::unix::fs::PermissionsExt;
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

fn socket_path(pid: u32) -> PathBuf {
    singleton::default_owner_dir().join(format!("restart-{pid}.sock"))
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
        let path = socket_path(std::process::id());
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let listener = UnixListener::bind(&path)?;
        let endpoint = Self { listener, path };
        std::fs::set_permissions(&endpoint.path, std::fs::Permissions::from_mode(0o600))?;
        Ok(endpoint)
    }

    pub async fn accept(&self) -> io::Result<std::os::unix::net::UnixStream> {
        loop {
            let (mut peer, _) = self.listener.accept().await?;
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
                Ok::<_, io::Error>(())
            };
            match tokio::time::timeout(TIMEOUT, valid).await {
                Ok(Ok(())) => {
                    let peer = peer.into_std()?;
                    peer.set_nonblocking(false)?;
                    peer.set_write_timeout(Some(TIMEOUT))?;
                    return Ok(peer);
                }
                _ => tracing::warn!("refused invalid restart request"),
            }
        }
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Sending SCM_RIGHTS is not confirmation that recvmsg installed the descriptor.
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
    anyhow::ensure!(incumbent.kind == ProxyKind::Tcr,
        "the incumbent cannot hand over its listener; stop it explicitly once to upgrade; --replace left it running");
    let mut peer = UnixStream::connect(socket_path(incumbent.pid)).await.context(
        "the incumbent does not offer listener handoff; stop it explicitly once to upgrade; --replace left it running",
    )?;
    verify_same_user(&peer)?;
    peer.write_all(REQUEST).await?;
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
