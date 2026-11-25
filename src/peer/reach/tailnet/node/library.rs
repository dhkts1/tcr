//! The `tailscale` crate behind [`super::TailnetDevice`].

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};

use super::{Accepted, AuthKey, Authorization, NodeKeyFile, NodeSelf};

/// Builds `tailscale::Device`s.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct LibraryFactory;

/// One `tailscale::Device`.
pub(crate) struct LibraryDevice(tailscale::Device);

/// One `tailscale` TCP listener.
pub(crate) struct LibraryListener(tailscale::netstack::TcpListener);

impl super::TailnetListener for LibraryListener {
    type Stream = tailscale::netstack::TcpStream;

    async fn accept(&self) -> Result<Accepted<Self::Stream>> {
        let stream = self
            .0
            .accept()
            .await
            .map_err(|err| anyhow!("the node's listener: {err}"))?;
        Ok(Accepted {
            from: stream.remote_addr(),
            bind: stream.local_addr(),
            stream,
        })
    }
}

impl super::TailnetDevice for LibraryDevice {
    type Stream = tailscale::netstack::TcpStream;
    type Listener = LibraryListener;

    async fn authorization(&self) -> Result<Authorization> {
        match self.0.is_authorized().await.context("asking control")? {
            tailscale::AuthState::Authorized => Ok(Authorization::Authorized),
            // The login URL is not used: version one is auth keys only
            // (upstream #383), and the URL is not something to log.
            tailscale::AuthState::NotAuthorized(_url) => Ok(Authorization::NotAuthorized),
        }
    }

    async fn self_node(&self) -> Result<NodeSelf> {
        let node = self.0.self_node().await.context("asking for this node")?;
        let address = node.tailnet_address.ipv4.addr();
        Ok(NodeSelf {
            address,
            name: node.hostname,
        })
    }

    async fn listen(&self, addr: SocketAddr) -> Result<Self::Listener> {
        self.0
            .tcp_listen(addr)
            .await
            .map(LibraryListener)
            .with_context(|| format!("listening on {addr}"))
    }

    async fn shutdown(self, deadline: Duration) -> bool {
        self.0.shutdown(Some(deadline)).await
    }

    async fn connect(&self, addr: SocketAddr) -> Result<Self::Stream> {
        self.0
            .tcp_connect(addr)
            .await
            .with_context(|| format!("connecting to {addr} through the tailnet"))
    }
}

/// The library's keys out of the key file.
///
/// Through text, not `serde_json::from_value`: the library's key types
/// deserialize only from a BORROWED string, which an owned `Value` cannot
/// lend ("expected a borrowed string"). And the parse error is dropped,
/// not wrapped, because serde quotes the value it choked on, which here is
/// a private key.
pub(super) fn persist_state(keys: &NodeKeyFile) -> Result<tailscale::keys::PersistState> {
    let text = serde_json::to_string(&keys.key_state)
        .map_err(|_| anyhow!("the tailnet key file's keys could not be read"))?;
    serde_json::from_str(&text).map_err(|_| {
        anyhow!(
            "the tailnet key file's keys are not in the form this build of the library \
             reads (written by another version?)"
        )
    })
}

impl super::DeviceFactory for LibraryFactory {
    type Device = LibraryDevice;
    const SUPPORTED: bool = true;

    fn mint_keys(&self) -> Result<serde_json::Value> {
        serde_json::to_value(tailscale::keys::PersistState::default())
            .context("serializing fresh tailnet keys")
    }

    async fn build(&self, keys: &NodeKeyFile, auth_key: Option<AuthKey>) -> Result<LibraryDevice> {
        // Both rustls providers are compiled in with this feature, and
        // rustls then refuses to choose: the library's control client
        // calls `ClientConfig::builder()`, which panics unless a default
        // is installed. Installed here, the one place a device is built,
        // so no order of calls elsewhere can get ahead of it.
        crate::base::http_client::install_crypto_provider();
        let key_state = persist_state(keys)?;
        let config = tailscale::Config {
            key_state,
            client_name: Some("tcr".to_string()),
            requested_hostname: keys.hostname.clone(),
            ..tailscale::Config::default()
        };
        tailscale::Device::new(&config, auth_key.map(AuthKey::into_secret))
            .await
            .map(LibraryDevice)
            .context("starting the node")
    }
}
