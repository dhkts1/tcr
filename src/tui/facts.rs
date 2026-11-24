//! What the dashboard knows beyond the snapshot (the Codex route and traffic, the CA,
//! the codex login), read once per tick so drawing touches neither the filesystem nor the clock.

use std::path::PathBuf;

use tcr_status_wire::{CodexRoute, CodexTraffic};

use crate::manager::Manager;
use crate::provider::Provider;
use crate::stats::StatsSnapshot;

/// What the dashboard knows beyond the snapshot, filled once per tick by the run loop
/// ([`dashboard_facts`]) so drawing never touches the filesystem or the clock itself. Every
/// field is `None` unless the fleet calls for it, so a Claude-only dashboard reads nothing new.
#[derive(Debug, Clone, Default)]
pub(crate) struct DashboardFacts {
    /// Whether this process pools codex, and why not: the decision `tcr status` and
    /// `tcr run --codex` take too ([`crate::codex_route`]). Only while the fleet holds a Codex
    /// row.
    pub(crate) codex_route: Option<CodexRoute>,
    /// What this process did with Codex requests in the last ten minutes. Only while the fleet
    /// holds a Codex row.
    pub(crate) codex_traffic: Option<CodexTraffic>,
    /// tcr's CA, which the not-pooled `no-ca` reason names. With [`Self::codex_route`].
    pub(crate) ca_path: Option<PathBuf>,
    /// The codex CLI's own login file, when one is on disk: found by its existence alone, never
    /// read. Only for an empty fleet, whose pane points at it.
    pub(crate) codex_login: Option<PathBuf>,
    /// The home folder, which a path a person reads shows as `~`.
    pub(crate) home: Option<PathBuf>,
}

/// The facts for `snapshot`, each asked for only when the fleet calls for it, as every Codex
/// surface is gated: `route` (with the number of Codex rows) and `traffic` only when a Codex row
/// exists, and `codex_login` only on an empty fleet. So a Claude-only fleet's dashboard stats
/// no file and computes no route.
pub(super) fn dashboard_facts(
    snapshot: &StatsSnapshot,
    home: Option<PathBuf>,
    route: impl FnOnce(usize) -> (Option<CodexRoute>, Option<PathBuf>),
    traffic: impl FnOnce() -> Option<CodexTraffic>,
    codex_login: impl FnOnce() -> Option<PathBuf>,
) -> DashboardFacts {
    let codex_rows = snapshot
        .accounts
        .iter()
        .filter(|a| a.provider == Provider::Codex)
        .count();
    let (codex_route, ca_path) = if codex_rows > 0 {
        route(codex_rows)
    } else {
        (None, None)
    };
    DashboardFacts {
        codex_route,
        codex_traffic: if codex_rows > 0 { traffic() } else { None },
        ca_path,
        codex_login: if snapshot.accounts.is_empty() {
            codex_login()
        } else {
            None
        },
        home,
    }
}

/// [`dashboard_facts`] for the serving process: its own listener's report, through the one
/// in-process route decision (`codex_route::codex_route_in_process`), the CA and whether it is
/// readable as `tcr status` checks it, its own refusal rings, and codex's login file by its
/// existence alone.
pub(super) fn live_facts(manager: &Manager, snapshot: &StatsSnapshot) -> DashboardFacts {
    dashboard_facts(
        snapshot,
        std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .map(PathBuf::from),
        |codex_rows| {
            let ca = crate::mitm::minted_ca_path();
            let ca_readable = ca.is_file();
            let route = crate::codex_route::codex_route_in_process(
                &manager.mitm_status(),
                manager.proxy_port(),
                &ca,
                ca_readable,
                codex_rows,
                manager.upstream_for(Provider::Codex),
            );
            (Some(route), Some(ca))
        },
        || Some(manager.codex_traffic(crate::now_ms())),
        || {
            let login = crate::codex_local::codex_login_path();
            crate::codex_local::login_file_exists(&login).then_some(login)
        },
    )
}

/// Does the fleet hold an account of `provider`, in any state?
pub(super) fn holds(snapshot: &StatsSnapshot, provider: Provider) -> bool {
    snapshot.accounts.iter().any(|a| a.provider == provider)
}
