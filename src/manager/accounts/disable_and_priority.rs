//! Benching, erroring and re-prioritising one account, by index or by query.

use crate::manager::*;

/// What [`Manager::set_disabled_by_query`] did.
///
/// `Applied` carries the RESOLVED name plus the durable half's fate, which the
/// caller must surface — see [`DisablePersist::warning`].
///
/// The name is resolved, not echoed, so the answer says what was actually
/// parked. It is NOT a substring or case-insensitive match:
/// [`crate::identity::match_accounts`] is the account's exact name,
/// byte-for-byte and untrimmed. Do not "fix" resolution to match a looser
/// description; a widened rule is how a query silently parks an account nobody
/// named, which is exactly what the `Ambiguous` arm below exists to refuse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetDisabledOutcome {
    Applied {
        name: String,
        persist: DisablePersist,
    },
    /// The query named no account in the live rotation.
    NoMatch,
    /// The query named more than one — only reachable on a config hand-edited
    /// past the loader's rename migration. Listed here so the caller can tell
    /// the user which rows to go fix.
    Ambiguous(Vec<String>),
}

/// What [`Manager::set_priority_by_query`] did. Same resolution arms as
/// [`SetDisabledOutcome`], with no durable half: the CLI writes the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetPriorityOutcome {
    /// The RESOLVED name of the account whose priority is now in force.
    Applied { name: String },
    /// The query named no account in the live rotation.
    NoMatch,
    /// The query named more than one.
    Ambiguous(Vec<String>),
}

/// What [`Manager::set_disabled`] achieved about DURABILITY — whether the bench
/// will still be there after a restart.
///
/// Returned rather than only logged because `tracing` is redirected to a log file
/// in TUI mode, so a failed write was invisible to the one person who could act on
/// it: the TUI kept rendering the account as disabled while the flag had provably
/// not reached disk. Every non-`None` [`Self::warning`] is a state where pressing
/// `d` looked like it worked and did not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisablePersist {
    /// The config file carries the flag — written just now, or already correct.
    Persisted,
    /// No config file behind this manager (`tcr demo`, `tcr status --probe`, the
    /// tests). Memory-only BY DESIGN: nothing to persist to, nothing wrong.
    NoConfigFile,
    /// The index named no account, so nothing changed in memory either.
    NoSuchAccount,
    /// Nothing on disk carries this account's identity — the entry was deleted or
    /// renamed while the proxy ran.
    NoEntry,
    /// More than one entry carries this identity; the write was refused rather
    /// than landed on a guess. See [`config::DisabledWrite::Ambiguous`].
    Ambiguous,
    /// The write itself failed (unreadable, malformed, or unwritable file).
    WriteFailed,
}

impl DisablePersist {
    /// The one line to put in front of the user, or `None` when the outcome needs
    /// no explanation. Short enough for a single terminal row and phrased so the
    /// headline (`NOT SAVED`) survives truncation on a narrow pane.
    ///
    /// `disabled` is the DIRECTION the keypress asked for, and it is required
    /// because the consequence of a failed write is its exact opposite in the two
    /// directions. Nothing was written either way, so the file still says whatever
    /// it said: after a failed **disable** the account comes back IN ROTATION,
    /// after a failed **enable** it comes back BENCHED. A single direction-blind
    /// line told the user "it returns to rotation on restart" for both — so on a
    /// failed enable they restarted on that advice and got the opposite.
    pub const fn warning(self, disabled: bool) -> Option<&'static str> {
        match self {
            // The flag is durable, or was never meant to be (demo/status have no
            // config file at all) — nothing to say.
            Self::Persisted | Self::NoConfigFile => None,
            Self::NoSuchAccount => Some("NOT SAVED: that account row no longer exists"),
            Self::NoEntry => Some(if disabled {
                "NOT SAVED: no config entry matches this account — it returns to rotation on restart"
            } else {
                "NOT SAVED: no config entry matches this account — it stays benched after a restart"
            }),
            // Says nothing about a restart, so it needs no direction — only an
            // action that actually helps.
            Self::Ambiguous => Some(
                "NOT SAVED: two config entries share this account's identity — give each its own orgUuid",
            ),
            Self::WriteFailed => Some(if disabled {
                "NOT SAVED: writing the config failed — it returns to rotation on restart"
            } else {
                "NOT SAVED: writing the config failed — it stays benched after a restart"
            }),
        }
    }

    /// [`Self::warning`] for a row of `provider`. Only the duplicate notice differs: two
    /// entries that share a Codex login are told apart by neither of them holding it twice,
    /// so a Codex row names that remedy (remove one, or re-import it under a name of its own),
    /// where a Claude row keeps its `orgUuid` remedy. Every other outcome reads the same for
    /// both providers.
    pub const fn warning_for(
        self,
        disabled: bool,
        provider: crate::provider::Provider,
    ) -> Option<&'static str> {
        match provider {
            crate::provider::Provider::Anthropic => self.warning(disabled),
            crate::provider::Provider::Codex => match self {
                Self::Ambiguous => Some(
                    "NOT SAVED: two config entries share this Codex login — remove the duplicate \
                     (tcr remove <name>), or re-import it with --name",
                ),
                _ => self.warning(disabled),
            },
        }
    }
}

impl Manager {
    /// Flush account `idx`'s `disabled` flag to the config file, so an account
    /// deliberately benched from the TUI is still benched after a restart.
    ///
    /// Upholds the same three invariants as [`Self::persist_tokens`], with the line
    /// carrying each marked below:
    ///  - **INV1** — the `config_write` guard spans the whole read-modify-write, so
    ///    this write and a concurrent token rotation never interleave. That matters
    ///    more here than the flag does: an interleaved write can clobber a refresh
    ///    token that rotated in between, and a refresh token is single-use, so the
    ///    clobbered account 400s (`invalid_grant`) on its next refresh and is dead
    ///    until re-authed by hand.
    ///  - **INV2** — `save_disabled` runs with `self.config` NOT held. That lock is
    ///    on the per-connection path (`session_affinity_enabled`), so holding it
    ///    across a read + `sync_all` + rename let one keypress stall connection
    ///    setup.
    ///  - **INV3** — the in-memory snapshot is mutated only once the file is known
    ///    to carry the flag. Mutating first (and regardless of the outcome) left
    ///    memory and disk permanently diverged on `NoEntry`/`Ambiguous`/`Err`, with
    ///    nothing to reconcile or retry them — the exact opposite of the guarantee.
    ///
    /// Writes the flag ONLY, via [`config::save_disabled`], never the whole config:
    /// the in-memory `Config` is a boot-time snapshot, so flushing it whole would
    /// revert every setting the user edited while the proxy was running.
    ///
    /// A missing `config_path` (tests, `tcr demo`, `tcr status --probe`) is a
    /// SILENT no-op — those managers must never touch a real config file.
    fn persist_disabled(
        &self,
        idx: usize,
        target: &config::Account,
        disabled: bool,
    ) -> DisablePersist {
        let Some(path) = &self.config_path else {
            return DisablePersist::NoConfigFile;
        };
        // INV1 to INV3 live in `write_through`.
        let outcome = self.write_through(
            path,
            |path| config::save_disabled(path, target, disabled),
            |config| {
                // Resolved by identity, through the one `identity::resolve` that
                // `persist_tokens` and `save_disabled` resolve through, with the
                // provider riding on `target` (`set_disabled` builds it from the row),
                // so all three land on the same record and never on a row of another
                // provider. An unbreakable tie writes nothing: `save_disabled` refuses
                // one too, and memory must not drift onto a record the file was not
                // allowed to touch.
                if let crate::identity::Resolved::One(position) =
                    crate::identity::resolve(config.accounts.iter().enumerate(), target)
                    && let Some(account) = config.accounts.get_mut(position)
                {
                    account.disabled = if disabled { Some(true) } else { None };
                }
            },
        );
        match outcome {
            Ok(config::DisabledWrite::Updated) => {
                tracing::info!(
                    account = %target.name,
                    index = idx,
                    disabled,
                    "persisted account disabled flag to config"
                );
                DisablePersist::Persisted
            }
            Ok(config::DisabledWrite::Unchanged) => {
                tracing::debug!(
                    account = %target.name,
                    index = idx,
                    disabled,
                    "config already carries this disabled state; nothing written"
                );
                DisablePersist::Persisted
            }
            Ok(config::DisabledWrite::NoEntry) => {
                tracing::warn!(
                    account = %target.name,
                    index = idx,
                    path = %path.display(),
                    "no config entry carries this account's identity; the disabled flag will NOT survive a restart"
                );
                DisablePersist::NoEntry
            }
            Ok(config::DisabledWrite::Ambiguous) => {
                tracing::warn!(
                    account = %target.name,
                    index = idx,
                    path = %path.display(),
                    "more than one config entry carries this account's identity; refusing to guess which one to flag, so the disabled flag will NOT survive a restart"
                );
                DisablePersist::Ambiguous
            }
            Err(err) => {
                tracing::error!(
                    error = %err,
                    account = %target.name,
                    index = idx,
                    path = %path.display(),
                    "failed to persist the disabled flag to config"
                );
                DisablePersist::WriteFailed
            }
        }
    }

    /// Sideline account `idx` on a proven-dead credential. Arms the recovery backoff
    /// with it, so even a hand-sidelined row is re-probed rather than stuck forever.
    pub fn mark_error(&self, idx: usize) {
        {
            let mut accounts = self
                .accounts
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some(account) = accounts.get_mut(idx) {
                account.status = AccountStatus::Error;
            }
        }
        self.grow_error_backoff(idx);
    }

    /// Arm (or double) the recovery backoff for an errored row. First rejection
    /// arms [`ERROR_REPROBE_BASE_MS`]; each subsequent one doubles up to
    /// [`ERROR_REPROBE_CAP_MS`], so a permanently dead credential is retried at most
    /// ~twice an hour instead of hammering the OAuth endpoint (which would risk
    /// rate-limiting the whole fleet).
    pub(in crate::manager) fn grow_error_backoff(&self, idx: usize) {
        let mut accounts = self
            .accounts
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(account) = accounts.get_mut(idx) {
            account.error_backoff_ms = if account.error_backoff_ms == 0 {
                ERROR_REPROBE_BASE_MS
            } else {
                account
                    .error_backoff_ms
                    .saturating_mul(2)
                    .min(ERROR_REPROBE_CAP_MS)
            };
            account.error_retry_after_ms = Some(crate::now_ms() + account.error_backoff_ms);
        }
    }

    /// Enable/disable account `idx`. Re-enabling clears a stuck error/hold.
    ///
    /// The flag is also PERSISTED (see [`Self::persist_disabled`]) — memory-only
    /// was the bug: a restart silently returned a deliberately benched account to
    /// rotation, because the server writes nothing but credentials back.
    ///
    /// Returns what became of the DURABLE half, because in TUI mode `tracing` is
    /// redirected to a log file: a failed write reported only there leaves the TUI
    /// rendering the account as benched while the bench provably will not survive a
    /// restart. The caller is expected to put [`DisablePersist::warning`] in front of
    /// the person who pressed the key.
    pub fn set_disabled(&self, idx: usize, disabled: bool) -> DisablePersist {
        // Take the identity out under the accounts lock and RELEASE that lock
        // before persisting. The persist path takes the config lock, and holding
        // both at once here would invert the order `warm_targets` reads them in
        // (config, then accounts) — the shape a deadlock is made of.
        let target = {
            let mut accounts = self
                .accounts
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            // A row deleted from the config keeps its slot, parked and unlisted, and
            // `disabled` is all that keeps it out of rotation (`eligible` never reads
            // `removed`), so it answers as a row that no longer exists: enabling it would
            // serve an account the operator deleted, on its in-memory credentials, until a
            // restart. Checked under this write lock, so no reload can mark or revive the
            // row between the check and the write. Reached by a dashboard key drawn before
            // the delete, and by `set_disabled_by_query`, which resolves under a read lock
            // it releases before calling here.
            let Some(account) = accounts.get_mut(idx).filter(|account| !account.removed) else {
                return DisablePersist::NoSuchAccount;
            };
            account.disabled = disabled;
            if !disabled && account.status == AccountStatus::Error {
                account.status = AccountStatus::Active;
                account.rate_limited_until_ms = None;
            }
            // `save_disabled` locates the row by this probe, and
            // `identity::same_identity` never matches a row of the other provider:
            // a probe built with the wrong provider (Anthropic) addresses no Codex
            // row, so a Codex account's bench would never reach disk.
            crate::identity::probe(
                account.provider,
                &account.name,
                account.account_uuid.clone(),
                account.org_uuid.clone(),
                account.org_name.clone(),
            )
        };
        self.persist_disabled(idx, &target, disabled)
    }

    /// Resolve a user-supplied `(query, org)` against the LIVE rotation and apply
    /// `disabled` to whatever it names — the whole operation the control endpoint
    /// performs, kept here so no caller outside this module has to know that a
    /// rotation index is what [`Self::set_disabled`] takes.
    ///
    /// Resolution runs over `self.accounts` — the rotation slots themselves — and
    /// NOT over the boot-time config snapshot, so the index handed to
    /// `set_disabled` is a slot by construction. Matching the config's vector and
    /// then indexing the runtime one would be correct only while the two stay the
    /// same length in the same order forever, which is not a property anything
    /// here enforces.
    ///
    /// The read guard is released before `set_disabled` takes its write guard —
    /// `RwLock` is not reentrant, so holding it across the call would deadlock.
    pub fn set_disabled_by_query(&self, query: &str, disabled: bool) -> SetDisabledOutcome {
        let (idx, name) = {
            let accounts = self.accounts.read().unwrap_or_else(PoisonError::into_inner);
            match match_listed(&accounts[..], query) {
                crate::identity::Match::One(idx) => (idx, accounts[idx].name.clone()),
                crate::identity::Match::None => return SetDisabledOutcome::NoMatch,
                crate::identity::Match::Ambiguous(names) => {
                    return SetDisabledOutcome::Ambiguous(names);
                }
            }
        };
        SetDisabledOutcome::Applied {
            name,
            persist: self.set_disabled(idx, disabled),
        }
    }

    /// Resolve `query` against the LIVE rotation and set that account's
    /// `priority` in memory, so the next selection ranks by it. For the
    /// [`crate::proxy::PRIORITY_PATH`] route behind `tcr priority`.
    ///
    /// Memory only, deliberately unlike [`Self::set_disabled`]: `tcr priority`
    /// computes the value from the config file (its `--first`/`--last` are
    /// relative to the file's priorities) and writes the file itself, then
    /// sends the resolved integer here. Resolution is the same
    /// [`crate::identity::match_one`] over the rotation slots that
    /// [`Self::set_disabled_by_query`] uses, for the same reason.
    pub fn set_priority_by_query(&self, query: &str, priority: i64) -> SetPriorityOutcome {
        let mut accounts = self
            .accounts
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        let idx = match match_listed(&accounts[..], query) {
            crate::identity::Match::One(idx) => idx,
            crate::identity::Match::None => return SetPriorityOutcome::NoMatch,
            crate::identity::Match::Ambiguous(names) => {
                return SetPriorityOutcome::Ambiguous(names);
            }
        };
        let account = &mut accounts[idx];
        account.priority = priority;
        SetPriorityOutcome::Applied {
            name: account.name.clone(),
        }
    }
}
