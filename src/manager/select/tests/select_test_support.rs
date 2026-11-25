//! The setup this file's test modules share: one way to build a manager,
//! a healthy account, and the session-pin map read and written directly.
use super::*;
use crate::base::config::Account;
use crate::clients::oauth::NoRefresh;
use crate::clients::warmer::LiveWarmer;
use crate::records::probe::LiveUsageProber;

/// A healthy Anthropic account, the same shape every module in this file builds.
pub(super) fn account(name: &str) -> Account {
    Account::fixture(name)
}

pub(super) fn config_with(accounts: Vec<Account>) -> Config {
    Config::fixture(accounts)
}

/// A manager over `config` with the live prober and warmer and no refresher or
/// config file.
pub(super) fn live_manager(config: Config) -> Arc<Manager> {
    live_manager_with(config, None)
}

/// [`live_manager`], with `path` as the config file behind it.
pub(super) fn live_manager_on_file(config: Config, path: PathBuf) -> Arc<Manager> {
    live_manager_with(config, Some(path))
}

fn live_manager_with(config: Config, path: Option<PathBuf>) -> Arc<Manager> {
    Manager::new(
        config,
        Arc::new(NoRefresh),
        Arc::new(LiveUsageProber::new()),
        Arc::new(LiveWarmer::new()),
        path,
    )
}

/// Seed a live pin directly: session `key` on account `idx`, last touched at
/// `touched_ms`.
pub(super) fn pin(manager: &Manager, key: u64, idx: usize, touched_ms: i64) {
    manager
        .affinity
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(key, (idx, touched_ms));
}

/// The account session `key` is pinned to, if any.
pub(super) fn pin_of(manager: &Manager, key: u64) -> Option<usize> {
    manager
        .affinity
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&key)
        .map(|&(idx, _)| idx)
}
