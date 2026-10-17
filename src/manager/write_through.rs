//! The one order a targeted config write and its in-memory mirror run in.
//!
//! A surgical writer in [`crate::config`] changes one key of the file; the
//! running `Manager` then mirrors that key into its `Config` snapshot. Three
//! invariants hold the two together, and [`Manager::write_through`] is where
//! they live:
//!
//!  - **INV1** — the `config_write` guard spans the whole read-modify-write, so
//!    this write and a concurrent token rotation never interleave. An
//!    interleaved write can clobber a refresh token that rotated in between,
//!    and a refresh token is single-use, so the clobbered account 400s
//!    (`invalid_grant`) on its next refresh and is dead until re-authed by hand.
//!  - **INV2** — the disk write runs with `self.config` NOT held. That lock is on
//!    the per-connection path (`session_affinity_enabled`), so holding it across
//!    a read + `sync_all` + rename let one keypress stall connection setup.
//!  - **INV3** — the in-memory snapshot is mutated only once the file is known
//!    to carry the requested state ([`config::Edit::landed`]). A hand copy that
//!    mutated first, and regardless of the outcome, left memory and disk
//!    permanently diverged on `NoEntry`/`Ambiguous`/`Err`, with nothing to
//!    reconcile or retry them.
//!
//! Writers whose memory change IS the write's content (`persist_tokens`) cannot
//! follow INV3 and keep their own order.

use std::path::Path;
use std::sync::PoisonError;

use crate::config::{self, Config, ConfigError};

use super::Manager;

impl Manager {
    /// Run `disk` against the config file at `path` under `config_write`, then
    /// run `apply` on the in-memory snapshot when the outcome says the file now
    /// carries the requested state. Returns `disk`'s outcome for the caller to
    /// report.
    pub(super) fn write_through<E: config::Edit>(
        &self,
        path: &Path,
        disk: impl FnOnce(&Path) -> Result<E, ConfigError>,
        apply: impl FnOnce(&mut Config),
    ) -> Result<E, ConfigError> {
        // INV1 — held across `disk`'s whole read + modify + sync + rename.
        let _writing = self
            .config_write
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // INV2 — no `self.config` guard is alive on this line, so the file I/O
        // cannot block a per-connection config read.
        let outcome = disk(path);
        // INV3 — memory is touched only when the FILE now carries the requested
        // state. `Unchanged` qualifies: the document already said exactly this,
        // so nothing was written precisely because nothing needed to be.
        if outcome.as_ref().is_ok_and(config::Edit::landed) {
            apply(&mut self.config.lock().unwrap_or_else(PoisonError::into_inner));
        }
        outcome
    }
}
