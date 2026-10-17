//! A real `tcr` proxy served in-process, for a test whose code under test dials `proxy.port`.

use std::path::PathBuf;
use std::sync::Arc;

use crate::config::Config;
use crate::manager::Manager;

/// Serve the proxy for `config` on `listener` for the rest of the test, with `config_path` as
/// the file behind it (`None`: the proxy persists nothing). Answers its manager, so a test can
/// read or change the state the proxy serves from.
///
/// The listener comes from the caller ([`crate::test_support::loopback::listener`]), because
/// the port is usually written into `config` before this is called.
pub fn serve(
    listener: tokio::net::TcpListener,
    config: Config,
    config_path: Option<PathBuf>,
) -> Arc<Manager> {
    let manager = Manager::with_live_refresher(config, config_path);
    let served = Arc::clone(&manager);
    tokio::spawn(async move { crate::mitm::serve(listener, served, None).await });
    manager
}
