//! Adding an account to the live rotation or replacing its credentials, and persisting it.

use super::*;

/// What [`Manager::add_or_update_account`] did.
///
/// Same shape as [`SetDisabledOutcome`] — resolve by identity against the LIVE
/// rotation, then report what happened plus the durable half's fate — but with
/// one difference `SetDisabledOutcome` never needs: a disable/enable query only
/// ever names an account that must already exist, so a miss is failure
/// (`NoMatch`). An account-add legitimately names one nobody has seen before, so
/// `Match::None` here is the SUCCESS arm (`Added`), not a rejection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddAccountOutcome {
    /// No account in the live rotation carried this identity — appended at the
    /// END via [`Manager::add_account`]. Pins are in-memory `(session_key,
    /// account_index)`, so appending is the only insertion that cannot re-key a
    /// live session onto the wrong account. An account with no explicit
    /// `priority` is assigned `max(existing priorities) + 1` — deliberate,
    /// not incidental: it joins the back of the fleet rather than reading as
    /// 0 (the primary tier) — see the doc-comment on the Added arm inside
    /// [`Manager::add_or_update_account`].
    Added {
        idx: usize,
        name: String,
        persist: AddPersist,
    },
    /// Exactly one account already carried this identity — its credentials were
    /// replaced IN PLACE at `idx` (see [`Manager::add_or_update_account`]). Its
    /// `account_type` is always stamped to the submitted value and any identity
    /// field (`account_uuid`/`org_uuid`/`org_name`) it was missing is backfilled
    /// — never a value it already carried. Routing state — priority, disabled
    /// flag, switch threshold, learned quota, counters — is untouched, and
    /// `idx` never moves.
    Updated {
        idx: usize,
        name: String,
        persist: AddPersist,
    },
    /// The submitted identity matches more than one account in the live
    /// rotation; these are their names, so the caller can tell the operator
    /// what to disambiguate with. Never guessed.
    Ambiguous(Vec<String>),
}

/// What [`Manager::add_or_update_account`] achieved about DURABILITY. Same role
/// as [`DisablePersist`], deliberately simpler: an upsert either lands or it does
/// not, and there is no "no such account" / "no config entry" arm here — a MISS
/// on the durable side is FILLED IN by [`config::save_account`], never refused
/// (see that function's doc-comment for why it differs from
/// [`config::save_disabled`] on exactly this point).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddPersist {
    /// The config file carries the new or replaced credentials — written just
    /// now.
    Persisted,
    /// No config file behind this manager (`tcr demo`, `tcr status --probe`,
    /// tests). Memory-only BY DESIGN.
    NoConfigFile,
    /// More than one entry on disk carries this identity; the write was refused
    /// rather than landed on a guess. See [`config::AccountWrite::Ambiguous`].
    Ambiguous,
    /// The write itself failed: unreadable, malformed or unwritable file, or a
    /// document whose `accounts` key exists but is not a JSON array.
    WriteFailed,
}

impl AddPersist {
    /// The one line to put in front of the operator, or `None` when the outcome
    /// needs no explanation.
    pub const fn warning(self) -> Option<&'static str> {
        match self {
            Self::Persisted | Self::NoConfigFile => None,
            Self::Ambiguous => Some(
                "NOT SAVED: another config entry shares this account's identity — give it its own orgUuid",
            ),
            Self::WriteFailed => {
                Some("NOT SAVED: writing the config failed — this will not survive a restart")
            }
        }
    }
}

impl Manager {
    /// Append a new account to the live rotation and return its index.
    ///
    /// APPEND-ONLY — this is a hard requirement, not a style choice. Pins are
    /// in-memory `(session_key, account_index)`; appending keeps every existing
    /// index valid, whereas inserting or reordering would re-key live sessions
    /// and cold-start their prompt cache, which is the exact cost this whole
    /// feature exists to avoid. Never insert at a position other than the end,
    /// and never reorder either vec.
    ///
    /// Updates both `self.accounts` (the live rotation `AccountRuntime`s) and
    /// `self.config`'s accounts vec (so later identity-resolution in
    /// `persist_tokens` / `persist_disabled` finds the row instead of logging
    /// the "no loaded config account carries this identity" WARN). The two
    /// locks are taken SEQUENTIALLY, never nested — the same discipline
    /// `set_disabled` documents: holding both at once risks inverting the lock
    /// order `warm_targets` reads them in.
    ///
    /// Derives the runtime row via [`AccountRuntime::from_config`], which is
    /// exactly how every other account's runtime state is built at startup, so
    /// an appended account gets its own `refresh_lock` for free — there is no
    /// longer a parallel structure to keep in sync.
    pub fn add_account(&self, account: config::Account) -> usize {
        let http1_only = self
            .config
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .http1_only;
        let runtime = AccountRuntime::from_config(&account, http1_only);
        let idx = {
            let mut accounts = self
                .accounts
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            accounts.push(runtime);
            accounts.len() - 1
        };
        {
            let mut config = self.config.lock().unwrap_or_else(PoisonError::into_inner);
            config.accounts.push(account);
        }
        idx
    }

    /// Resolve `account` against the LIVE rotation by identity and either append
    /// it (nothing matched) or replace an existing account's credentials in
    /// place (exactly one matched) — the whole operation `POST /_tcr/accounts`
    /// performs, kept here for the same reason [`Self::set_disabled_by_query`]
    /// is: no caller outside this module needs to know that a rotation index is
    /// what the mutating primitives take.
    ///
    /// Resolution uses [`crate::identity::resolve`]/[`crate::identity::same_identity`]
    /// — the SAME rule the durable half ([`config::save_account`]) and `tcr
    /// login` (`oauth::upsert_account`) already run — never
    /// [`crate::identity::match_one`]. `match_one` goes through
    /// [`crate::identity::Queryable`], which exposes only the name: it
    /// cannot compare `account_uuid`, so it was structurally unable to tell two
    /// different people sharing a display name apart. Unifying on `resolve`
    /// fixes that (a differing `account_uuid` never matches, so a same-named
    /// different person is APPENDED, never overwritten) and it fixes the
    /// live/durable split brain the old rule caused: `resolve` tolerates an
    /// unknown org on either side exactly as the durable write does, so a
    /// legacy no-org row and a submission carrying its own org now agree on
    /// which one account that is, on both halves, every time — not only when
    /// there happen to be two orgs in play.
    ///
    /// A submission carrying no identity fields at all (a bare name) still
    /// finds its row: `same_identity` falls back to name equality when either
    /// side has no uuid, and names are unique, so `resolve` answers that case
    /// on its own. It used to need a `match_one` retry on top, because a bare
    /// email had to find a stored `email (Org)` display name — a shape that
    /// existed only while names were not unique.
    ///
    /// Resolve AND mutate under ONE write-lock acquisition of `self.accounts` —
    /// closing a TOCTOU the old two-lock version had: it resolved under a READ
    /// lock, released it, then took a separate WRITE lock to append. Two
    /// concurrent submissions of the same brand-new identity could both resolve
    /// "no match" before either had appended, and both would append —
    /// duplicating the live row. Holding one write lock across resolve-then-
    /// mutate means the second caller's resolve runs after the first caller's
    /// append is already visible, so it finds the row and updates it instead.
    ///
    /// `config_write` is held across the WHOLE function — resolve, mutate, and
    /// persist — not just the file I/O at the end. The `self.accounts` fix
    /// above only serializes the LIVE half: it guarantees the second of two
    /// concurrent same-identity submissions sees the first one's row and takes
    /// the Updated path instead of duplicating it. It said nothing about the
    /// DURABLE half, which used to take its own `config_write` lock only
    /// inside `persist_added`/`persist_replaced`, well after `self.accounts`
    /// was released — so two callers' persists could still race onto
    /// `config_write` in the OPPOSITE order from the one their live resolves
    /// agreed on, each writing its OWN submitted credentials. The loser's
    /// `persist_replaced` (seeing nothing on disk yet) would append a fresh
    /// entry with ITS tokens; the winner's `persist_added` would then find
    /// that entry and merge ITS OWN tokens over it — leaving the file holding
    /// whichever call happened to reach `config_write` first, independent of
    /// which one actually won the live race. Measured 2/200 over 200 rounds of
    /// two concurrent adds of one identity. Holding this lock across both
    /// halves makes resolve-mutate-persist one atomic unit, so the durable
    /// write order always agrees with the live resolve order.
    ///
    /// This is the one place in `Manager` that holds `config_write` and
    /// `self.accounts` at once — everywhere else they are taken sequentially,
    /// never nested (see `set_disabled`'s and `warm_targets`'s comments on
    /// that discipline). It is safe: `self.accounts` is held only for the
    /// brief in-memory resolve-and-mutate inside this span, never across the
    /// file I/O below, and no other code path holds `self.accounts` and then
    /// reaches for `config_write` or `config` — so this adds one new,
    /// one-directional edge (`config_write` → `self.accounts`) to the lock
    /// order, not a cycle. `persist_added`/`persist_replaced` no longer take
    /// `config_write` themselves — they assume the caller already holds it.
    ///
    /// A resolution that lands on Added needs a freshly-built
    /// [`AccountRuntime`] (a `reqwest::Client`) to push, and building one must
    /// never happen while EITHER lock is held — a panic mid-build would
    /// poison it for every other `.expect(...)` site on that lock,
    /// `config_write` included (`persist_tokens`, `persist_now`'s shutdown
    /// flush, this function's own next call). So `config_write` is held
    /// across each ATTEMPT's resolve/mutate/persist, not necessarily across
    /// the whole call: an attempt that resolves Added with no runtime ready
    /// yet mutates NOTHING, drops both locks, builds, and retries — see the
    /// loop inside the function body for why at most one retry is ever
    /// needed and why it cannot reopen the TOCTOU above.
    pub fn add_or_update_account(&self, mut account: config::Account) -> AddAccountOutcome {
        // The submission's provider rides on the probe, so a Codex submission never
        // resolves to an Anthropic row that shares its name, live or on disk
        // (`identity::same_identity`): it is added beside it, never stamped onto it.
        let target = crate::identity::probe(
            account.provider,
            &account.name,
            account.account_uuid.clone(),
            account.org_uuid.clone(),
            account.org_name.clone(),
        );

        /// What the locked resolve-and-mutate section below produced, so the
        /// durable write can run after `self.accounts`'s lock is released —
        /// `persist_added`/`persist_replaced` do their own file I/O and must
        /// never run under `self.accounts`'s lock (though both still run
        /// under `config_write`, held since this attempt's start — see the N2
        /// doc-comment on `add_or_update_account`).
        enum Resolution {
            Added {
                idx: usize,
            },
            Updated {
                idx: usize,
                name: String,
                /// The row's OWN identity (post-backfill) plus its REAL routing
                /// state (`priority/switch_threshold/disabled`) — never
                /// `identity::probe`'s placeholders, see [`Self::persist_replaced`]
                /// and the F5 note there. Boxed: `Added` is a bare `usize`, and
                /// clippy flags the size gap between variants otherwise.
                target: Box<config::Account>,
            },
            /// The resolve below landed on Added, but there is no
            /// already-built [`AccountRuntime`] to push yet — see the loop
            /// below, which built nothing here on purpose.
            NeedsBuild,
        }

        // Built on demand, never while a lock is held — see the doc-comment
        // above `add_or_update_account`. `None` until the first attempt
        // below discovers it actually needs one.
        let mut speculative_runtime: Option<AccountRuntime> = None;

        loop {
            // N2 — see above: held across THIS ATTEMPT's resolve, mutate and
            // persist so it never interleaves with another concurrent call's.
            // Dropped early, before any mutation, on an attempt that turns out
            // to need a client it doesn't have yet (`NeedsBuild` below).
            let _writing = self
                .config_write
                .lock()
                .unwrap_or_else(PoisonError::into_inner);

            let resolution = {
                let mut accounts = self
                    .accounts
                    .write()
                    .unwrap_or_else(PoisonError::into_inner);

                let probes: Vec<(usize, config::Account)> = accounts
                    .iter()
                    .enumerate()
                    .map(|(i, a)| {
                        (
                            i,
                            crate::identity::probe(
                                a.provider,
                                &a.name,
                                a.account_uuid.clone(),
                                a.org_uuid.clone(),
                                a.org_name.clone(),
                            ),
                        )
                    })
                    .collect();

                let matched =
                    match crate::identity::resolve(probes.iter().map(|(i, p)| (*i, p)), &target) {
                        crate::identity::Resolved::One(idx) => Some(idx),
                        crate::identity::Resolved::Many => {
                            // Recompute the loose set for its names: `Resolved::Many`
                            // itself carries none, and this is exactly the set
                            // `resolve` drew from (same predicate, same candidates).
                            let names = accounts
                                .iter()
                                .filter(|a| {
                                    crate::identity::same_identity(
                                        &target,
                                        &crate::identity::probe(
                                            a.provider,
                                            &a.name,
                                            a.account_uuid.clone(),
                                            a.org_uuid.clone(),
                                            a.org_name.clone(),
                                        ),
                                    )
                                })
                                .map(|a| a.name.clone())
                                .collect();
                            return AddAccountOutcome::Ambiguous(names);
                        }
                        crate::identity::Resolved::None => None,
                    };

                match matched.and_then(|idx| accounts.get_mut(idx).map(|row| (idx, row))) {
                    Some((idx, row)) => {
                        row.access_token.clone_from(&account.access_token);
                        row.refresh_token.clone_from(&account.refresh_token);
                        row.expires_at_ms = account.expires_at.map(oauth::normalize_expires_at);
                        // F4: fresh credentials clear a stuck error — `eligible`
                        // hard-gates on `status == Error` — but do NOT clear a
                        // rate-limit hold. That hold expires on its own
                        // (`MAX_RATE_LIMIT_HOLD_SECONDS`) and the next 429 re-arms
                        // it; clearing it here bought nothing but the ability to
                        // race a genuinely-still-limited account back into
                        // rotation early.
                        if row.status == AccountStatus::Error {
                            row.status = AccountStatus::Active;
                        }
                        // A login of an account deleted while this process ran
                        // lands on the deleted row's slot (same identity) and
                        // brings it back as the login asked for it, not parked
                        // the way the delete left it. The durable write below
                        // puts the row back in the file, so the next reload
                        // agrees.
                        if row.removed {
                            row.removed = false;
                            row.disabled = account.disabled.unwrap_or(false);
                        }
                        // A Codex re-login brings a live login, so whatever killed the last one
                        // no longer holds, and the folder it came from (or none, for a login
                        // of tcr's own) is the submission's. Whatever the row's status was, so
                        // neither sits inside the two conditions above.
                        match account.provider {
                            crate::provider::Provider::Anthropic => {}
                            crate::provider::Provider::Codex => {
                                row.login_error = None;
                                row.codex_home.clone_from(&account.codex_home);
                                row.pending_codex_writeback = None;
                            }
                        }
                        // F6: the submitted type always wins — this route only
                        // ever carries a real credential, so a row whose stored
                        // type was stale (e.g. `"api"`) is corrected, or
                        // `refresh_plan` silently never refreshes it again.
                        // Identity fields are BACKFILLED — filled in only where
                        // the row does not already carry a value — mirroring
                        // `oauth::upsert_account`. Backfilling (rather than
                        // ignoring) is what permanently closes the split-brain
                        // trigger: once a legacy no-org row's org is filled in,
                        // it stops being loosely matched by every org variant of
                        // that person and starts requiring an exact org match,
                        // like any other fully-known account.
                        row.account_type = account.account_type.clone();
                        if row.account_uuid.is_none() {
                            row.account_uuid.clone_from(&account.account_uuid);
                        }
                        if row.org_uuid.is_none() {
                            row.org_uuid.clone_from(&account.org_uuid);
                        }
                        if row.org_name.is_none() {
                            row.org_name.clone_from(&account.org_name);
                        }
                        // Plan fields are OVERWRITTEN where the submission has
                        // one, not backfilled like the identity fields above —
                        // a plan changes (an org upgrades, an account joins a
                        // Team) while an identity does not. A `None` still
                        // writes nothing, so a re-add that carried no profile
                        // never blanks a plan we already know. Same rule as
                        // `config::merge_account`'s durable half; see
                        // `config::Account::organization_type`.
                        if account.organization_type.is_some() {
                            row.organization_type.clone_from(&account.organization_type);
                        }
                        if account.rate_limit_tier.is_some() {
                            row.rate_limit_tier.clone_from(&account.rate_limit_tier);
                        }
                        if account.seat_tier.is_some() {
                            row.seat_tier.clone_from(&account.seat_tier);
                        }
                        if account.plan_display_name.is_some() {
                            row.plan_display_name.clone_from(&account.plan_display_name);
                        }
                        // F5: carry the row's REAL routing state, not
                        // `identity::probe`'s None placeholders — see
                        // `persist_replaced`'s doc-comment for the restart-un-bench
                        // this replaces.
                        //
                        // `egress`/`egress_strict` are NOT on `AccountRuntime` (see
                        // `Self::account_egress`'s doc-comment: a pin is operator
                        // intent that only the config file states), so the only
                        // place to read the row's current pin from is
                        // `self.config`, resolved by identity, the same way
                        // `persist_replaced` resolves its own write target, never
                        // by `idx`, because `idx` here is a `self.accounts`
                        // (runtime) index and is not guaranteed to line up with
                        // `self.config.accounts`.
                        let existing_pin = {
                            let config = self.config.lock().unwrap_or_else(PoisonError::into_inner);
                            match crate::identity::resolve(
                                config.accounts.iter().enumerate(),
                                &target,
                            ) {
                                crate::identity::Resolved::One(position) => config
                                    .accounts
                                    .get(position)
                                    .map_or_default(config::Account::egress_pin),
                                crate::identity::Resolved::Many
                                | crate::identity::Resolved::None => config::EgressPin::default(),
                            }
                        };
                        let target = config::Account {
                            name: row.name.clone(),
                            account_type: row.account_type.clone(),
                            // The SUBMISSION's provider: `persist_replaced`
                            // writes through `config::locate_account_entry`,
                            // which never matches a row of the other provider,
                            // so a default (Anthropic) here would append an
                            // Anthropic row carrying a Codex credential instead
                            // of updating the Codex row in place.
                            provider: account.provider,
                            account_uuid: row.account_uuid.clone(),
                            org_uuid: row.org_uuid.clone(),
                            org_name: row.org_name.clone(),
                            access_token: String::new(),
                            refresh_token: None,
                            expires_at: None,
                            priority: Some(row.priority),
                            switch_threshold: row.switch_threshold,
                            disabled: row.disabled.then_some(true),
                            // Same "carry the row's REAL routing state" rule as
                            // priority/switch_threshold/disabled above: group labels
                            // are configured routing state, not identity, so a
                            // credential re-add/refresh must not silently clear them.
                            groups: (!row.groups.is_empty()).then(|| row.groups.clone()),
                            // Carried for the same reason as `groups` above,
                            // with one difference: these were just merged from
                            // the submission a few lines up, so `row` already
                            // holds the newest reading and the durable write
                            // gets it rather than the pre-merge value.
                            organization_type: row.organization_type.clone(),
                            rate_limit_tier: row.rate_limit_tier.clone(),
                            seat_tier: row.seat_tier.clone(),
                            plan_display_name: row.plan_display_name.clone(),
                            // Carried from the config row this account already
                            // resolves to, never hardcoded to `Local`/unpinned;
                            // see the `existing_pin` note above. A re-add that
                            // supplied no egress preference of its own must not
                            // silently drop an operator's exit-node pin.
                            egress: existing_pin.egress,
                            egress_strict: existing_pin.strict,
                            // The row's folder, just set from the submission: a row revived
                            // from a delete has no file entry left, so the write appends this
                            // whole `target`, and the folder reaches the file only through it.
                            codex_home: row.codex_home.clone(),
                            extra: serde_json::Map::new(),
                        };
                        Resolution::Updated {
                            idx,
                            name: row.name.clone(),
                            target: Box::new(target),
                        }
                    }
                    None => match speculative_runtime.take() {
                        // No runtime ready yet — mutate nothing, report it, and
                        // let the loop below build one with no lock held before
                        // retrying. Safe to abandon this attempt outright: it
                        // has not touched `accounts`, `self.config`, or disk.
                        None => Resolution::NeedsBuild,
                        Some(mut runtime) => {
                            // A follow-up regression on this arm (distinct from the
                            // TOCTOU fix this function is named for above): an
                            // appended account with no explicit priority must join
                            // the BACK of the fleet — `max(existing priorities) + 1`
                            // — not the 0 that `AccountRuntime::from_config`'s
                            // `unwrap_or(0)` reads from an absent priority, which
                            // would silently promote it to the PRIMARY tier ahead of
                            // the established fleet. Computed from `accounts` (the
                            // LIVE rotation, not the config snapshot) because this
                            // section already holds its write lock and it is
                            // authoritative for what is routing right now. Skipped
                            // when the caller submitted an explicit priority (the
                            // documented new-account case — see `AddAccountRequest`'s
                            // doc-comment in `proxy.rs`), which is never overridden.
                            // Mirrors the identical fix in `config::merge_account`'s
                            // Added arm.
                            if account.priority.is_none() {
                                let next_priority = accounts
                                    .iter()
                                    .map(|a| a.priority)
                                    .max()
                                    .map_or(0, |max| max + 1);
                                account.priority = Some(next_priority);
                                // Built on the PRIOR attempt, before `next_priority`
                                // was known — patch it in rather than rebuilding
                                // (which would call `build_serving_client()` again,
                                // under this same lock, resurrecting exactly the bug
                                // the loop above exists to avoid).
                                runtime.priority = next_priority;
                            }
                            accounts.push(runtime);
                            Resolution::Added {
                                idx: accounts.len() - 1,
                            }
                        }
                    },
                }
            };

            match resolution {
                Resolution::NeedsBuild => {
                    // Both locks are released here (self.accounts already went
                    // out of scope above; config_write is dropped explicitly)
                    // before the one call that can panic — see the doc-comment
                    // above `add_or_update_account`. Nothing was mutated on this
                    // attempt, so retrying from scratch is safe; the next
                    // attempt is guaranteed to have a runtime ready, so it
                    // cannot hit this arm again.
                    drop(_writing);
                    let http1_only = self
                        .config
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .http1_only;
                    speculative_runtime = Some(AccountRuntime::from_config(&account, http1_only));
                    continue;
                }
                Resolution::Added { idx } => {
                    // Still sequential, never nested, with `self.accounts` (which
                    // is already released by this point) — the `self.config` lock
                    // here is a separate, short critical section, same discipline
                    // `add_account` documents. It runs nested inside
                    // `config_write` (held since this attempt's start, see the N2
                    // doc-comment above), which is what makes this whole arm
                    // atomic with respect to another concurrent call.
                    {
                        let mut config = self.config.lock().unwrap_or_else(PoisonError::into_inner);
                        config.accounts.push(account.clone());
                    }
                    let name = account.name.clone();
                    let persist = self.persist_added(&account);
                    return AddAccountOutcome::Added { idx, name, persist };
                }
                Resolution::Updated { idx, name, target } => {
                    let persist = self.persist_replaced(idx, &target, &account);
                    return AddAccountOutcome::Updated { idx, name, persist };
                }
            }
        }
    }

    /// Durably persist a newly-appended account (see [`Self::add_account`]) to
    /// the config file. Same INV1 discipline as [`Self::persist_disabled`]: the
    /// whole `config::save_account` read-modify-write runs under
    /// `config_write`, never `self.config` — nothing here needs the in-memory
    /// snapshot, because [`Self::add_account`] already pushed `account` onto it.
    ///
    /// PRECONDITION: unlike every other `config_write` writer in this module,
    /// this one does NOT take the lock itself — its only caller,
    /// [`Self::add_or_update_account`], already holds it across the whole
    /// resolve-mutate-persist sequence (see that function's N2 doc-comment),
    /// and `Mutex` is not reentrant, so locking again here would deadlock.
    /// Never call this without `config_write` already held.
    fn persist_added(&self, account: &config::Account) -> AddPersist {
        let Some(path) = &self.config_path else {
            return AddPersist::NoConfigFile;
        };
        match config::save_account(path, account) {
            Ok(config::AccountWrite::Added) => {
                tracing::info!(account = %account.name, "persisted newly added account to config");
                AddPersist::Persisted
            }
            Ok(config::AccountWrite::Updated) => {
                // The on-disk document already carried this identity even
                // though the live rotation did not — an entry added to the
                // file by hand while the proxy ran, or a stale boot snapshot.
                // The fresh credentials still landed, just as an update to
                // that row rather than a new one.
                tracing::info!(
                    account = %account.name,
                    "an on-disk entry already carried this identity; its credentials were updated instead of appending a duplicate"
                );
                AddPersist::Persisted
            }
            Ok(config::AccountWrite::Ambiguous) => {
                tracing::warn!(
                    account = %account.name,
                    path = %path.display(),
                    "more than one config entry carries this account's identity; refusing to guess, so the new account will NOT survive a restart"
                );
                AddPersist::Ambiguous
            }
            Ok(config::AccountWrite::Unwritable) => {
                tracing::error!(
                    account = %account.name,
                    path = %path.display(),
                    "the config document's accounts key is not a JSON array; refusing to touch it"
                );
                AddPersist::WriteFailed
            }
            Err(err) => {
                tracing::error!(
                    error = %err,
                    account = %account.name,
                    path = %path.display(),
                    "failed to persist the newly added account to config"
                );
                AddPersist::WriteFailed
            }
        }
    }

    /// Durably persist replaced credentials for the account at live index `idx`,
    /// identified by `target` — its OWN stored identity (post-backfill), never
    /// the submitted request's (see [`Self::add_or_update_account`] for why).
    ///
    /// `target` also carries the row's REAL `priority`/`switch_threshold`/
    /// `disabled`, not `identity::probe`'s `None` placeholders. That matters
    /// only on the rare path below where no in-memory or on-disk entry carries
    /// this identity and one gets APPENDED from `target`: a placeholder-routing
    /// `target` would append a fresh, un-benched entry for an account that was
    /// deliberately disabled, so it would come back into rotation on the very
    /// next restart — the exact bug `persist_disabled` exists to prevent.
    ///
    /// PRECONDITION: same as [`Self::persist_added`] — does NOT take
    /// `config_write` itself; its only caller, [`Self::add_or_update_account`],
    /// already holds it. Never call this without `config_write` already held.
    fn persist_replaced(
        &self,
        idx: usize,
        target: &config::Account,
        fresh: &config::Account,
    ) -> AddPersist {
        let Some(path) = &self.config_path else {
            return AddPersist::NoConfigFile;
        };
        let mut for_disk = target.clone();
        for_disk.access_token.clone_from(&fresh.access_token);
        for_disk.refresh_token.clone_from(&fresh.refresh_token);
        for_disk.expires_at = fresh.expires_at;
        match fresh.provider {
            crate::provider::Provider::Anthropic => {}
            crate::provider::Provider::Codex => {
                for_disk.extra.remove(config::CODEX_SYNC_PENDING_KEY);
            }
        }

        let outcome = config::save_account(path, &for_disk);
        // INV3, mirroring `persist_disabled`: touch the in-memory snapshot only
        // once the FILE actually carries the new credentials. Without this, the
        // snapshot `persist_tokens`/`persist_now` later clone and write back on
        // shutdown would still hold the OLD (just-replaced) token and stamp it
        // back over the file this call just fixed.
        if matches!(
            outcome,
            Ok(config::AccountWrite::Updated | config::AccountWrite::Added)
        ) {
            let mut config = self.config.lock().unwrap_or_else(PoisonError::into_inner);
            match crate::identity::resolve(config.accounts.iter().enumerate(), target) {
                crate::identity::Resolved::One(position) => {
                    if let Some(entry) = config.accounts.get_mut(position) {
                        entry.access_token.clone_from(&for_disk.access_token);
                        entry.refresh_token.clone_from(&for_disk.refresh_token);
                        entry.expires_at = for_disk.expires_at;
                        entry.codex_home.clone_from(&for_disk.codex_home);
                        entry.extra.remove(config::CODEX_SYNC_PENDING_KEY);
                    }
                }
                // No in-memory entry carries this identity either — the disk
                // row `save_account` just appended has no counterpart here.
                // Mirror the append so a later persist can still find it.
                crate::identity::Resolved::None => config.accounts.push(for_disk.clone()),
                // A tie the in-memory snapshot cannot break either; leave it
                // alone rather than guess which entry to overwrite.
                crate::identity::Resolved::Many => {}
            }
        }

        match outcome {
            Ok(config::AccountWrite::Updated) => {
                tracing::info!(account = %target.name, index = idx, "persisted replaced credentials to config");
                AddPersist::Persisted
            }
            Ok(config::AccountWrite::Added) => {
                tracing::warn!(
                    account = %target.name,
                    index = idx,
                    path = %path.display(),
                    "no config entry carried this account's identity; a fresh entry was appended"
                );
                AddPersist::Persisted
            }
            Ok(config::AccountWrite::Ambiguous) => {
                tracing::warn!(
                    account = %target.name,
                    index = idx,
                    path = %path.display(),
                    "more than one config entry carries this account's identity; refusing to guess, so the replaced credentials will NOT survive a restart"
                );
                AddPersist::Ambiguous
            }
            Ok(config::AccountWrite::Unwritable) => {
                tracing::error!(
                    account = %target.name,
                    index = idx,
                    path = %path.display(),
                    "the config document's accounts key is not a JSON array; refusing to touch it"
                );
                AddPersist::WriteFailed
            }
            Err(err) => {
                tracing::error!(
                    error = %err,
                    account = %target.name,
                    index = idx,
                    path = %path.display(),
                    "failed to persist replaced credentials to config"
                );
                AddPersist::WriteFailed
            }
        }
    }
}
