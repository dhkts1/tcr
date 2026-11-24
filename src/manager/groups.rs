//! The group settings the manager routes by, held as one value.
//!
//! Every fact here comes from the config file's `groupSettings` and account
//! `groups`, and a hot reload ([`super::Manager::reload_groups_if_changed`])
//! replaces all of them at once. The manager keeps them behind one
//! `RwLock<Arc<GroupPolicy>>`, so a reader clones one `Arc` and sees one
//! version of the file: a `select` can never read `reserved` from one reload
//! and `spillToPool` from the next. The `Manager` methods at the bottom read
//! that one value and run the hot reload.

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, PoisonError};

use crate::config::{self, Config};
use crate::oauth;

use super::{AccountRuntime, Manager};

/// One version of the fleet's group settings.
#[derive(Debug, Default, PartialEq)]
pub(super) struct GroupPolicy {
    /// Groups marked `reserved` (`tcr group reserve`): an account carrying
    /// one is off-limits to traffic that asked for no group. See
    /// [`super::Manager::reserved_blocks`].
    pub(super) reserved: HashSet<String>,
    /// Groups marked `parked` (`tcr group park`): every member is out of
    /// rotation. See [`super::Manager::parked_blocks`].
    pub(super) parked: HashSet<String>,
    /// Groups opted in to `allowControlAccount`: an explicit ask for one may
    /// select the control account.
    pub(super) control_allowed: HashSet<String>,
    /// Groups opted in to `spillToPool`: an explicit ask for one falls back to
    /// the whole pool when no member can serve.
    pub(super) spill: HashSet<String>,
    /// Every group on the fleet mapped to its resolved color, in wire order
    /// (`config::Config::group_colors`).
    pub(super) colors: BTreeMap<String, String>,
}

impl GroupPolicy {
    pub(super) fn from_config(config: &Config) -> Self {
        Self {
            reserved: config.reserved_group_names(),
            parked: config.parked_group_names(),
            control_allowed: config.control_allowed_group_names(),
            spill: config.spill_group_names(),
            colors: config.group_colors(),
        }
    }

    /// Whether an explicit ask for `group` is held strictly inside it:
    /// `reserved || !spill`. A reserved group ignores `spillToPool`; see
    /// [`crate::config::GroupSettings::spill_to_pool`].
    pub(super) fn is_strict(&self, group: &str) -> bool {
        self.reserved.contains(group) || !self.spill.contains(group)
    }

    /// One line per setting that differs between `self` and `fresh`, in the
    /// words [`super::Manager::apply_group_reload`] logs. Empty when nothing
    /// changed.
    pub(super) fn changes_to(&self, fresh: &Self) -> Vec<String> {
        let sets = [
            ("reserved groups", &self.reserved, &fresh.reserved),
            ("parked groups", &self.parked, &fresh.parked),
            (
                "control-allowed groups",
                &self.control_allowed,
                &fresh.control_allowed,
            ),
            ("spill-to-pool groups", &self.spill, &fresh.spill),
        ];
        let mut changed: Vec<String> = sets
            .into_iter()
            .filter(|(_, old, new)| old != new)
            .map(|(label, old, new)| {
                format!(
                    "{label}: {:?} -> {:?}",
                    sorted_groups(old),
                    sorted_groups(new)
                )
            })
            .collect();
        if self.colors != fresh.colors {
            changed.push(format!(
                "group colors: {:?} -> {:?}",
                self.colors, fresh.colors
            ));
        }
        changed
    }
}

/// The members of `groups` that are in `set`, sorted: the per-account
/// `reserved_groups` / `parked_groups` / `control_allowed_groups` a snapshot
/// row carries.
pub(super) fn carried_in(groups: &[String], set: &HashSet<String>) -> Vec<String> {
    let mut carried: Vec<String> = groups
        .iter()
        .filter(|g| set.contains(*g))
        .cloned()
        .collect();
    carried.sort();
    carried
}

/// A sorted `Vec` of `set`'s members, for a deterministic (and readable)
/// before/after in the reload's change log: `HashSet`'s own iteration order is
/// unspecified and would make the same reload log differently run to run.
fn sorted_groups(set: &HashSet<String>) -> Vec<String> {
    let mut v: Vec<String> = set.iter().cloned().collect();
    v.sort();
    v
}

impl Manager {
    /// The current group settings, one version of the config file. Clones the
    /// `Arc` rather than returning a guard so a caller never holds this lock
    /// across a second lock acquisition (`accounts`, `affinity`). A caller that
    /// asks more than one group question reads them all from one `groups()`,
    /// so a reload between two questions cannot hand it a mix of two versions.
    pub(super) fn groups(&self) -> Arc<GroupPolicy> {
        Arc::clone(&self.groups.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// The currently-reserved group names, for a caller that needs the set
    /// itself. Two calls to this and [`Self::parked_groups`] may read two
    /// versions; a caller that needs both reads [`Self::groups`] once.
    pub(super) fn reserved_groups(&self) -> HashSet<String> {
        self.groups().reserved.clone()
    }

    /// The currently-parked group names. See [`Self::reserved_groups`].
    pub(super) fn parked_groups(&self) -> HashSet<String> {
        self.groups().parked.clone()
    }

    /// Whether `group` is currently reserved — and therefore STRICT: its own
    /// traffic never spills to a non-member (see
    /// [`Self::select_with_group`]'s strict arm).
    ///
    /// A membership test rather than [`Self::reserved_groups`] + `contains`,
    /// because the request path asks this per request and only ever needs the
    /// one answer; cloning the set to look at a single key is waste on a hot
    /// path. Reads the same hot-reloaded cache, so `tcr group unreserve` takes
    /// effect on the next request with no restart.
    pub fn is_group_reserved(&self, group: &str) -> bool {
        self.groups().reserved.contains(group)
    }

    /// The group names currently opted in to `spillToPool`. See
    /// [`Self::reserved_groups`].
    pub(super) fn spill_groups(&self) -> HashSet<String> {
        self.groups().spill.clone()
    }

    /// Re-read [`config::Account::groups`] and `groupSettings` (`reserved`,
    /// `parked`, `allowControlAccount`, `color`) from `self.config_path` when the file's mtime has moved since
    /// the last check — the fix for group edits appearing to do nothing. The
    /// same pass also follows the file's account LIST: an account it gained is
    /// admitted, and one it lost is marked [`AccountRuntime::removed`], which is
    /// what makes a delete leave the running proxy and the panel without a
    /// restart. Called from a natural
    /// cadence point ([`Self::select_with_group`], [`Self::select_revalidation`],
    /// [`Self::snapshot`]) rather than a dedicated watcher thread — one more
    /// background task is one more thing to keep alive, and every one of
    /// those call sites already runs on every request or every TUI tick.
    ///
    /// Touches ONLY the fields named above. Every other field of `self.config`
    /// — accounts, credentials, tokens, every unmodelled top-level key — is
    /// left exactly as booted: the in-memory snapshot stays authoritative for
    /// those, or this would fight [`Self::persist_now`] / [`Self::persist_tokens`]
    /// / [`Self::persist_disabled`]'s read-modify-write of the SAME file.
    ///
    /// Checks the cached mtime before touching `config_write`. When a file change
    /// needs reloading, takes that lock without waiting and checks mtime again under
    /// the guard, keeping the same read/parse serialization as config writers.
    /// Requests use the last completed snapshot during an in-flight write or fsync;
    /// the next cadence check after the writer releases applies its completed edit.
    ///
    /// A file that cannot be stat'd, read, or parsed **keeps every current
    /// in-memory value** and logs a warning — never blanks the fleet's groups
    /// — and, unlike a successful reload, does NOT advance the remembered
    /// mtime, so a transient or malformed edit self-heals on the very next
    /// natural-cadence check rather than waiting for the file to change
    /// again.
    pub(super) fn reload_groups_if_changed(&self) {
        let Some(path) = &self.config_path else {
            return;
        };
        let read_mtime = || match std::fs::metadata(path).and_then(|m| m.modified()) {
            Ok(mtime) => Some(mtime),
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    path = %path.display(),
                    "group reload: could not stat config file, keeping current groups"
                );
                None
            }
        };
        let Some(mtime) = read_mtime() else {
            return;
        };
        if *self
            .groups_reload_mtime
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            == Some(mtime)
        {
            return;
        }
        let _writing = match self.config_write.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::WouldBlock) => return,
            Err(std::sync::TryLockError::Poisoned(_)) => panic!("config write lock poisoned"),
        };
        let Some(mtime) = read_mtime() else {
            return;
        };
        if *self
            .groups_reload_mtime
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            == Some(mtime)
        {
            return;
        }
        match config::load(path) {
            Ok(fresh) => {
                self.apply_group_reload(&fresh);
                *self
                    .groups_reload_mtime
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = Some(mtime);
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    path = %path.display(),
                    "group reload: config is unreadable/malformed, keeping current groups"
                );
            }
        }
    }

    /// Apply `fresh`'s group membership/settings onto the running server and
    /// log exactly what changed — the line an operator greps for after
    /// editing groups to ask "did it pick up my edit". Silent when nothing
    /// actually differs (the common case: most cadence ticks see an unchanged
    /// file, or a mtime bump from an unrelated write). Called only from
    /// [`Self::reload_groups_if_changed`], under `config_write`.
    ///
    /// The group settings are swapped as one [`GroupPolicy`], and only when
    /// one of them changed. They are applied BEFORE per-account `groups`, so
    /// that by the time this returns and the caller re-tests a pinned
    /// session's hard gate, a session on an account whose group just became
    /// reserved is judged against the fresh reservation — the existing
    /// pin-reclaim-on-reservation test (`pin_reclaim_when_the_pinned_accounts_group_is_reserved`)
    /// extends to the reload path for free, because [`Self::select_with_group`]
    /// re-tests [`Self::account_hard_ok`] against a fresh [`Self::groups`]
    /// snapshot on every call — reservation was never cached PAST one call, only
    /// ACROSS them until this reload existed.
    ///
    /// Matches each running [`AccountRuntime`] to `fresh.accounts` by
    /// [`crate::identity::same_identity`] — the SAME identity rule
    /// [`Self::persist_tokens`] uses — rather than by name, so a config
    /// mid-way through a login churn (a display name changed, an org backfilled)
    /// still reloads the right row instead of silently dropping its groups.
    pub(super) fn apply_group_reload(&self, fresh: &Config) {
        let fresh_groups = GroupPolicy::from_config(fresh);
        let mut changed: Vec<String> = {
            let mut groups = self.groups.write().unwrap_or_else(PoisonError::into_inner);
            let changed = groups.changes_to(&fresh_groups);
            if !changed.is_empty() {
                *groups = Arc::new(fresh_groups);
            }
            changed
        };
        {
            let mut accounts = self
                .accounts
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            let fresh_control = fresh.control_account.as_ref().map(|c| c.name().to_string());
            for (idx, account) in accounts.iter_mut().enumerate() {
                let probe = crate::identity::probe(
                    account.provider,
                    &account.name,
                    account.account_uuid.clone(),
                    account.org_uuid.clone(),
                    account.org_name.clone(),
                );
                let Some(matched) = fresh.find_same_identity(&probe) else {
                    // The file no longer holds this account: it was deleted.
                    // Parked in memory only (there is no entry left to write
                    // the flag into) and dropped from every snapshot; see
                    // `AccountRuntime::removed`.
                    if !account.removed {
                        account.removed = true;
                        account.disabled = true;
                        changed.push(format!("{}: removed from the config", account.name));
                        // A deleted account is not the control account: its
                        // routing bypasses `disabled` on purpose, so leaving the
                        // index would keep control traffic on the deleted row.
                        // Same lock order as `probeable_indices`: accounts, then
                        // control.
                        let mut control = self
                            .control_idx
                            .write()
                            .unwrap_or_else(PoisonError::into_inner);
                        if *control == Some(idx) {
                            *control = None;
                            changed
                                .push(format!("{}: no longer the control account", account.name));
                        }
                    }
                    continue;
                };
                if account.removed {
                    // Back in the file (an undone hand edit, a login that wrote
                    // the file): the file's row is the truth again, credentials
                    // and bench flag included.
                    account.removed = false;
                    account.disabled = matched.disabled.unwrap_or(false);
                    account.access_token.clone_from(&matched.access_token);
                    account.refresh_token.clone_from(&matched.refresh_token);
                    account.expires_at_ms = matched.expires_at.map(oauth::normalize_expires_at);
                    // The folder too. `login_error` is left as it is, as `status` is: a restored
                    // entry holds whatever tokens it held, and the next refresh decides.
                    account.codex_home.clone_from(&matched.codex_home);
                    changed.push(format!("{}: back in the config", account.name));
                    if fresh_control.as_deref() == Some(account.name.as_str()) {
                        let mut control = self
                            .control_idx
                            .write()
                            .unwrap_or_else(PoisonError::into_inner);
                        if control.is_none() {
                            *control = Some(idx);
                        }
                    }
                }
                let new_groups = matched.groups.clone().unwrap_or_default();
                if new_groups != account.groups {
                    changed.push(format!(
                        "{}: groups {:?} -> {:?}",
                        account.name, account.groups, new_groups
                    ));
                    account.groups = new_groups;
                }
            }

            // Admit an account the file gained since boot (or the last reload)
            // — APP-35. Never touches an account already in `accounts`: one
            // the file LOST was marked `removed` by the loop above, in place,
            // and one that came back was revived there too, so neither is
            // admitted a second time here. The identity match is the same probe the loop above uses, just
            // run in the opposite direction: does any LOADED account already
            // own this fresh entry?
            //
            // Pushed onto `self.config.accounts` in the SAME iteration as
            // `self.accounts` (never one without the other): `Self::account_egress`
            // and every other by-index reader of `self.config` assume the two
            // vectors are index-aligned, because accounts are appended, never
            // removed, from either. Before this fix only the runtime vector grew
            // here, so after one admission `self.accounts[idx]` and
            // `self.config.accounts.get(idx)` named two different accounts (or
            // `idx` was simply out of bounds on the config side) for every index
            // from the admitted account onward: a strict pin on any of them
            // silently failed open (`account_egress` returns `None` on an
            // out-of-range `idx`, and callers read `None` as "no pin").
            let mut config = self.config.lock().unwrap_or_else(PoisonError::into_inner);
            let http1_only = config.http1_only;
            config.admit_rows_from(fresh, |candidate| {
                let already_known = accounts.iter().any(|existing| {
                    let probe = crate::identity::probe(
                        existing.provider,
                        &existing.name,
                        existing.account_uuid.clone(),
                        existing.org_uuid.clone(),
                        existing.org_name.clone(),
                    );
                    crate::identity::same_identity(&probe, candidate)
                });
                if already_known {
                    return false;
                }
                changed.push(format!("{}: admitted by reload", candidate.name));
                accounts.push(AccountRuntime::from_config(candidate, http1_only));
                true
            });
        }

        if !changed.is_empty() {
            tracing::info!(changes = ?changed, "config reload: picked up a group edit");
        }
    }
}
