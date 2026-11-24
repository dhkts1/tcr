//! Mutation ownership and the flush of refreshed tokens to the config file.

use super::*;

impl Manager {
    /// Flush the refreshed TOKENS to disk. Token refreshes already persist
    /// incrementally via [`Self::persist_tokens`], so this is the
    /// belt-and-suspenders final flush on shutdown (DESIGN §main). A missing
    /// `config_path` (tests, corrupt-source boot) is a silent no-op so a corrupt
    /// user file is never clobbered with defaults.
    ///
    /// Writes via [`config::save_tokens`], NOT [`config::save`]: the in-memory
    /// `Config` is a boot-time snapshot, so flushing it whole would revert every
    /// setting the user edited while the proxy was running.
    /// Give up the right to mutate account state, permanently.
    ///
    /// Called by the predecessor in a socket handoff, AFTER its final flush and
    /// BEFORE the successor starts serving. See [`Self::mutation_released`] for
    /// what each refusal prevents. The release sits between the predecessor's
    /// last legitimate write and the successor's first: any earlier and the
    /// final flush is gated off by this very flag, any later and the
    /// predecessor's own shutdown persist lands on the successor's fresh
    /// tokens.
    pub fn release_mutation_ownership(&self) {
        // `swap` rather than `store` so the log line fires once even if a
        // caller releases twice.
        if !self.mutation_released.swap(true, Ordering::SeqCst) {
            tracing::info!(
                "mutation ownership released; this process will not refresh a token or write the config again"
            );
        }
    }

    /// Whether [`Self::release_mutation_ownership`] has been called.
    pub fn mutation_is_released(&self) -> bool {
        self.mutation_released.load(Ordering::SeqCst)
    }

    pub fn persist_now(&self) {
        // After a handoff this write would land our stale in-memory tokens on
        // top of the successor's fresh ones. `shutdown_within` calls this on
        // the way out, so the gate has to be here and not at the call site.
        if self.mutation_is_released() {
            tracing::debug!("skipping config persist: mutation ownership was released");
            return;
        }
        let Some(path) = &self.config_path else {
            return;
        };
        // INV1 — the write lock, held across the whole read-modify-write below, is
        // what stops a shutdown flush racing a concurrent persist_tokens and
        // clobbering a just-rotated token. It replaces the config lock in that role;
        // dropping it here while persist_tokens no longer holds `config` across its
        // save would leave BOTH writers unserialized.
        let _writing = self
            .config_write
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // INV2 — clone inside a short critical section so the config lock is
        // released on this line, before any file I/O below runs.
        let snapshot = self
            .config
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Err(err) = config::save_tokens(path, &snapshot) {
            tracing::error!(error = %err, "failed to flush config on shutdown");
        }
    }

    pub(super) fn persist_tokens(
        &self,
        name: &str,
        account_uuid: Option<String>,
        org_uuid: Option<String>,
        org_name: Option<String>,
        provider: crate::provider::Provider,
        tokens: &Tokens,
    ) {
        // Defence in depth: `refresh_plan` already refuses to produce a refresh
        // once ownership is released, so nothing should reach here. Gated anyway
        // because this is the function that actually touches the credential file.
        if self.mutation_is_released() {
            tracing::debug!("skipping token persist: mutation ownership was released");
            return;
        }
        let Some(path) = &self.config_path else {
            return;
        };
        // Match the config entry by full identity (account_uuid + org), not name
        // alone, so a rotated token lands on the RIGHT entry when the same email
        // is logged into two orgs (finding #9). With all identity fields None
        // (today's config) same_identity falls back to name equality — unchanged.
        //
        // The provider rides on the probe, as on every probe, and here it decides:
        // `identity::same_identity` never matches a row of another provider, so only
        // rows of this one are candidates, both in the `resolve` below and in the
        // pairing `save_tokens_for`'s merge makes. An Anthropic row and a Codex row
        // may share a name, and when either lacks an account UUID the name fallback
        // alone calls them one account — a tie that would refuse to persist this
        // rotation, or a pairing that would stamp it onto the other provider's row.
        let probe = crate::identity::probe(provider, name, account_uuid, org_uuid, org_name);
        let sync_pending = match provider {
            crate::provider::Provider::Anthropic => None,
            crate::provider::Provider::Codex => {
                let accounts = self.accounts.read().unwrap_or_else(PoisonError::into_inner);
                accounts
                    .iter()
                    .find(|a| {
                        a.provider == provider
                            && a.name == name
                            && a.account_uuid == probe.account_uuid
                            && a.org_uuid == probe.org_uuid
                    })
                    .and_then(|a| a.pending_codex_writeback.as_ref())
                    .map(refresh::PendingCodexWriteback::record)
            }
        };

        // INV1 — the modify and the save must be ONE atomic step against the file.
        // Without that, two concurrent refreshes race: a stale save clobbers the
        // other account's just-rotated refresh token, which then 400s
        // ("invalid_grant") on its next refresh and is dead until re-authed by hand.
        // This lock — not the config lock — is what provides that serialization now,
        // and it covers save_tokens' whole read-modify-write of the file.
        let _writing = self
            .config_write
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // INV2 — the config lock is taken ONLY for this block: apply the rotation to
        // the snapshot and clone it out. The guard drops at the closing brace, so
        // `save_tokens` below runs with `self.config` free and a per-connection
        // `session_affinity_enabled()` never waits on an fsync. Cloning is safe here
        // in a way it was NOT before: INV1 already serializes the writers, so no
        // second writer can slip a stale document in between.
        //
        // Deliberately NOT INV3 (mutate only after a successful write): the mutation
        // IS the write's content — it has to precede it — and memory is the recovery
        // buffer for a single-use token. If this save fails, the freshly rotated
        // token still sits in the snapshot for `persist_now` to flush at shutdown;
        // rolling memory back on failure would lose it for good.
        let (snapshot, placement) = {
            let mut config = self.config.lock().unwrap_or_else(PoisonError::into_inner);
            // Resolved, never first-match: `same_identity` treats an unknown org as
            // a match, so with the legacy two-org shape in the file a first-match
            // search lands THIS account's rotated credential on the OTHER account's
            // record — overwriting a single-use refresh token that is then dead.
            // An unbreakable tie writes nothing and says so.
            let placement = crate::identity::resolve(config.accounts.iter().enumerate(), &probe);
            if let crate::identity::Resolved::One(position) = placement
                && let Some(account) = config.accounts.get_mut(position)
            {
                account.access_token.clone_from(&tokens.access_token);
                // THE TRAP: same invariant as `apply_refresh` in
                // `manager/refresh.rs` — `tokens.refresh_token` must be
                // `Some(...)` on every reachable refresh success.
                account.refresh_token.clone_from(&tokens.refresh_token);
                account.expires_at = Some(tokens.expires_at_ms);
                match provider {
                    crate::provider::Provider::Anthropic => {}
                    crate::provider::Provider::Codex => match sync_pending {
                        Some(value) => {
                            account
                                .extra
                                .insert(config::CODEX_SYNC_PENDING_KEY.to_string(), value);
                        }
                        None => {
                            account.extra.remove(config::CODEX_SYNC_PENDING_KEY);
                        }
                    },
                }
            }
            (config.clone(), placement)
        };
        // Logged with the config lock released (INV2) — a `warn!` is cheap but the
        // critical section above is on the per-connection path's lock.
        match placement {
            crate::identity::Resolved::One(_) => {}
            crate::identity::Resolved::None => tracing::warn!(
                account = %name,
                "no loaded config account carries this identity; the rotated token is not in the snapshot and will not reach disk"
            ),
            crate::identity::Resolved::Many => tracing::warn!(
                account = %name,
                "more than one loaded config account carries this identity; refusing to guess which one just rotated, so the rotated token will not reach disk and this account may need `tcr login`"
            ),
        }
        // Tokens only, and only THIS account's: the in-memory config is a
        // boot-time snapshot, so writing it whole would stamp stale settings over
        // the user's live file. Its CREDENTIALS for the other accounts are stale
        // in the same way and cost more to lose, because another process that
        // refreshed or re-authed one of them in the meantime gets its single-use
        // refresh token reverted and that account dies until `tcr login`. See
        // `config::CredentialScope`.
        //
        // A placement that is not `One` means the rotated credential never
        // reached the snapshot at all (both branches above say exactly that), so
        // there is nothing of ours to persist and the only thing a write could do
        // is stamp stale copies over whatever is on disk. Write nothing.
        let crate::identity::Resolved::One(position) = placement else {
            return;
        };
        if let Err(err) =
            config::save_tokens_for(path, &snapshot, config::CredentialScope::Only(position))
        {
            tracing::error!(error = %err, "failed to persist refreshed token to config");
        }
    }
}
