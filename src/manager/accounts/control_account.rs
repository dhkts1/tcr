//! The control account: reading it, setting or clearing it by query, and persisting it.

use crate::manager::*;

/// What [`Manager::set_control_by_query`] did. Same shape as
/// [`SetDisabledOutcome`], with one difference: a `None` query is a legitimate
/// request (clear the control account), not a missing argument, so `Applied`'s
/// `name` is itself an `Option` rather than `Applied` only ever meaning "matched
/// something".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetControlOutcome {
    /// `name` is the resolved account name, or `None` when this call cleared
    /// the control account.
    Applied {
        name: Option<String>,
        persist: ControlPersist,
    },
    /// The query named no account in the live rotation.
    NoMatch,
    /// The query named more than one — only reachable on a config hand-edited
    /// past the loader's rename migration. Listed so the caller can tell the
    /// user which rows to go fix.
    Ambiguous(Vec<String>),
    /// The query named a Codex account, which cannot be the control account
    /// ([`codex_control_refusal`]); carries its name. Nothing was changed: the
    /// boot path ignores such a `controlAccount`, so accepting it here would
    /// persist a setting the next start throws away.
    CodexAccount(String),
}

/// Why a Codex account cannot be the control account, in the words both places
/// that meet one use: boot, which ignores a `controlAccount` naming one, and
/// `tcr control`, which refuses to set one. The control account is an Anthropic
/// identity (its bootstrap calls, its probe, its `controlIdentity` check), and
/// Codex has no counterpart yet.
pub const CODEX_HAS_NO_CONTROL_ACCOUNT: &str = "Codex has no control-account semantics yet";

/// `tcr control`'s refusal of the Codex account `name`, live
/// ([`SetControlOutcome::CodexAccount`]) and offline alike.
pub fn codex_control_refusal(name: &str) -> String {
    format!(
        "'{name}' is a Codex account, and {CODEX_HAS_NO_CONTROL_ACCOUNT}, so it cannot be the \
         control account."
    )
}

/// What [`Manager::set_control`] achieved about DURABILITY — whether the
/// control account will still be set (or cleared) after a restart. Same shape
/// as [`DisablePersist`] now that `controlAccount` is resolved by IDENTITY, not
/// by a bare name — see [`config::ControlWrite`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlPersist {
    /// The config file carries the control account (or its absence) — written
    /// just now, or already correct.
    Persisted,
    /// No config file behind this manager (`tcr demo`, `tcr status --probe`,
    /// tests). Memory-only BY DESIGN.
    NoConfigFile,
    /// More than one on-disk entry carries this identity; the write was
    /// refused rather than landed on a guess. See
    /// [`config::ControlWrite::Ambiguous`].
    Ambiguous,
    /// The write itself failed (unreadable, malformed, or unwritable file).
    WriteFailed,
}

impl ControlPersist {
    /// The one line to put in front of the user, or `None` when the outcome
    /// needs no explanation. Mirrors [`DisablePersist::warning`]; there is no
    /// direction-dependent wording here because clearing and setting fail the
    /// same way — a failed write just leaves the file saying whatever it said.
    pub const fn warning(self) -> Option<&'static str> {
        match self {
            Self::Persisted | Self::NoConfigFile => None,
            Self::Ambiguous => Some(
                "NOT SAVED: two config entries share this account's identity — give each its own orgUuid",
            ),
            Self::WriteFailed => {
                Some("NOT SAVED: writing the config failed — this will not survive a restart")
            }
        }
    }
}

impl Manager {
    /// The current control account's rotation index, or `None` when unset.
    pub fn control(&self) -> Option<usize> {
        *self
            .control_idx
            .read()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The current control account's name, or `None` when unset / the index no
    /// longer resolves (an account row cannot disappear — `add_account` is
    /// append-only — so the only way this reads `None` with `control()` some
    /// is a bug, not a live scenario; resolving defensively here rather than
    /// indexing avoids a panic either way).
    pub fn control_name(&self) -> Option<String> {
        let idx = self.control()?;
        self.account(idx, |a| a.name.clone())
    }

    /// Whether the control account also takes general inference traffic — the
    /// boot-time `controlPooled` snapshot. Surfaced on the `server started` line
    /// because it is the one routing flag whose cost is an OUTAGE rather than a
    /// slowdown, and an operator reading that line after a restart needs to see
    /// it without opening the config.
    pub const fn control_pooled(&self) -> bool {
        self.control_pooled
    }

    /// The boot-time `controlIdentity` snapshot — see [`crate::control_identity`].
    pub const fn control_identity(&self) -> config::ControlIdentityMode {
        self.control_identity
    }

    /// The identity the control account must be checked against: its `name`
    /// and `accountUuid`. `None` when no control account is set.
    pub fn control_identity_target(&self) -> Option<crate::control_identity::ControlIdentity> {
        let idx = self.control()?;
        self.account(idx, |a| crate::control_identity::ControlIdentity {
            name: a.name.clone(),
            account_uuid: a.account_uuid.clone(),
        })
    }

    /// The resolved-client-identity cache the check reads and fills.
    pub const fn client_identities(&self) -> &crate::control_identity::ClientIdentityCache {
        &self.client_identities
    }

    /// Persist ONLY the top-level `controlAccount` key, via
    /// [`config::save_control_account`] — never the whole config (see
    /// [`Self::persist_disabled`]'s doc-comment for why: the in-memory
    /// `Config` is a boot-time snapshot, and flushing it whole would revert
    /// every setting the user edited while the proxy ran — the exact clobber
    /// fixed in `1d978ce`).
    ///
    /// Same three invariants as [`Self::persist_disabled`]:
    ///  - **INV1** — `config_write` spans the whole read-modify-rename, so this
    ///    write and a concurrent token rotation or disabled-flag write can
    ///    never interleave.
    ///  - **INV2** — `save_control_account` runs with `self.config` NOT held,
    ///    so the file I/O never blocks the per-connection config read.
    ///  - **INV3** — the in-memory snapshot (`config.control_account`) is
    ///    mutated only once the file is known to carry the desired state
    ///    (`Updated` or `Unchanged`); a failed write leaves memory and disk in
    ///    the same (divergent-from-the-request) state rather than only disk.
    ///
    /// A missing `config_path` (tests, `tcr demo`, `tcr status --probe`) is a
    /// SILENT no-op — those managers must never touch a real config file.
    fn persist_control(&self, target: Option<config::Account>) -> ControlPersist {
        let Some(path) = &self.config_path else {
            return ControlPersist::NoConfigFile;
        };
        // INV1 to INV3 live in `write_through`.
        let outcome = self.write_through(
            path,
            |path| config::save_control_account(path, target.as_ref()),
            |config| config.name_control_account(target.as_ref()),
        );
        let name = target.as_ref().map(|a| a.name.as_str());
        match outcome {
            Ok(config::ControlWrite::Updated) => {
                tracing::info!(
                    control_account = ?name,
                    "persisted control account to config"
                );
                ControlPersist::Persisted
            }
            Ok(config::ControlWrite::Unchanged) => {
                tracing::debug!(
                    control_account = ?name,
                    "config already names this control account; nothing written"
                );
                ControlPersist::Persisted
            }
            Ok(config::ControlWrite::Ambiguous) => {
                tracing::error!(
                    control_account = ?name,
                    path = %path.display(),
                    "more than one config entry carries this account's identity; refusing to guess, so the control account will NOT survive a restart"
                );
                ControlPersist::Ambiguous
            }
            Err(err) => {
                tracing::error!(
                    error = %err,
                    control_account = ?name,
                    path = %path.display(),
                    "failed to persist the control account to config"
                );
                ControlPersist::WriteFailed
            }
        }
    }

    /// Set (`idx = Some(_)`) or clear (`idx = None`) the control account by
    /// rotation index, and persist it. Mirrors [`Self::set_disabled`]'s
    /// resolve-identity-then-persist shape: the identity is read out under a
    /// released `accounts` READ lock before `control_idx` is taken, so the two
    /// never nest (same discipline `set_disabled` documents for
    /// `accounts`/`config`).
    fn set_control(&self, idx: Option<usize>) -> ControlPersist {
        let target = idx.and_then(|i| {
            self.account(i, |a| {
                // Carries the account's provider for the reason
                // `set_disabled`'s probe does: `save_control_account`
                // checks the row through `config::locate_account_entry`.
                crate::identity::probe(
                    a.provider,
                    &a.name,
                    a.account_uuid.clone(),
                    a.org_uuid.clone(),
                    a.org_name.clone(),
                )
            })
        });
        {
            let mut control = self
                .control_idx
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            *control = idx;
        }
        self.persist_control(target)
    }

    /// Resolve a user-supplied `query` against the LIVE rotation and set
    /// the control account to whatever it names — the whole operation the
    /// control endpoint performs. `query = None` CLEARS the control account
    /// (there is nothing to resolve, so this never reaches `match_one`).
    ///
    /// Resolution runs over `self.accounts` — never the boot-time config
    /// snapshot — for the same reason [`Self::set_disabled_by_query`] does.
    pub fn set_control_by_query(&self, query: Option<&str>) -> SetControlOutcome {
        let Some(query) = query else {
            return SetControlOutcome::Applied {
                name: None,
                persist: self.set_control(None),
            };
        };
        let (idx, name) = {
            let accounts = self.accounts.read().unwrap_or_else(PoisonError::into_inner);
            match match_listed(&accounts[..], query) {
                // Refused here as boot ignores it (see `assemble`): a `match`, so a third
                // provider decides this too instead of inheriting an answer.
                crate::identity::Match::One(idx) => match accounts[idx].provider {
                    crate::provider::Provider::Anthropic => (idx, accounts[idx].name.clone()),
                    crate::provider::Provider::Codex => {
                        return SetControlOutcome::CodexAccount(accounts[idx].name.clone());
                    }
                },
                crate::identity::Match::None => return SetControlOutcome::NoMatch,
                crate::identity::Match::Ambiguous(names) => {
                    return SetControlOutcome::Ambiguous(names);
                }
            }
        };
        SetControlOutcome::Applied {
            name: Some(name),
            persist: self.set_control(Some(idx)),
        }
    }
}
