//! `Manager` state methods, split verbatim from `mod.rs`.

use super::*;

impl Manager {
    /// See [`crate::config::Config::probe_interval_seconds`].
    pub fn probe_interval_seconds(&self) -> u64 {
        self.config
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .probe_interval_seconds()
    }

    /// See [`crate::config::Config::warmup_interval_seconds`].
    pub fn warmup_interval_seconds(&self) -> u64 {
        self.config
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .warmup_interval_seconds()
    }

    /// Whether session affinity is enabled, read from the config's unmodelled
    /// top-level `sessionAffinity` (default `true` — ON unless explicitly
    /// disabled with `"sessionAffinity": false`). An absent key fails safe
    /// toward keeping caches warm: measured 2026-08-14, a config that lost its
    /// `sessionAffinity` key silently went cold when the default was `false`,
    /// and the client-side prompt-cache hit ratio dropped 0.952 → 0.498 with
    /// `cache_creation` rising 6.5M/hour → 122M/hour, sustained seven hours — a
    /// single missing key costing roughly 100M re-created tokens per hour.
    /// The read itself is [`crate::config::Config::session_affinity_enabled`],
    /// which `tcr config show` calls too. When explicitly
    /// disabled, the hybrid server injects no `SessionKey` extension, so
    /// `select` always receives `affinity = None` and the disabled path is
    /// inert.
    pub fn session_affinity_enabled(&self) -> bool {
        self.config
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .session_affinity_enabled()
    }

    /// See [`crate::config::Config::revalidation_serve_enabled`].
    pub fn revalidation_serve_enabled(&self) -> bool {
        self.config
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .revalidation_serve_enabled()
    }

    /// See [`crate::config::Config::load_balance_migration_enabled`].
    pub fn load_balance_migration_enabled(&self) -> bool {
        self.config
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .load_balance_migration_enabled()
    }

    /// See [`crate::config::Config::cache_ttl_rewrite_enabled`].
    pub fn cache_ttl_rewrite_enabled(&self) -> bool {
        self.config
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .cache_ttl_rewrite_enabled()
    }

    /// See [`crate::config::Config::divert_budget`].
    pub fn divert_budget(&self) -> u32 {
        self.config
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .divert_budget()
    }

    /// Whether this server's accounts are forced onto HTTP/1.1 — see
    /// [`config::Config::http1_only`]. Server-wide, not per-account, and
    /// exposed for the same reason `thresholds` is: a client must not
    /// re-derive this from `~/.config/tcr.json`, which may have been
    /// edited (or the process not yet restarted to pick it up) since the
    /// server actually booted its clients.
    pub fn http1_only(&self) -> bool {
        self.config
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .http1_only
    }

    /// Mint the next session key: a strictly-increasing, unique `u64` starting at
    /// 1. Called once per connection by the hybrid server when affinity is on.
    pub fn next_session_key(&self) -> u64 {
        self.session_seq.fetch_add(1, Ordering::Relaxed)
    }

    pub fn account_count(&self) -> usize {
        self.accounts
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// How many accounts belong to `provider`, in any state (disabled, held, rejected): every
    /// account that could ever serve a request of that provider, since `eligible` refuses the
    /// other provider's first. The proxy asks its "is the rotation spent" and "how many accounts
    /// are exhausted" questions of this and not of the fleet-wide [`Self::account_count`], so a
    /// mixed fleet answers an Anthropic request exactly as its Anthropic half alone would.
    ///
    /// Deleted slots never count: they cannot serve another request. Refresh the file's
    /// account list before counting so a request sees a completed live delete before selection
    /// runs. An in-flight file writer is never waited on by the request path.
    pub fn provider_account_count(&self, provider: crate::provider::Provider) -> usize {
        self.reload_groups_if_changed();
        self.accounts
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|a| a.provider == provider)
            .filter(|a| !a.removed)
            .count()
    }

    /// The live locked slot, when the lock belongs to this request's provider.
    /// A lock never disables another provider's independent pool.
    pub(crate) fn locked_account_for(&self, provider: crate::provider::Provider) -> Option<usize> {
        let idx = self.locked_idx?;
        self.account(idx, |account| {
            !account.removed && account.provider == provider
        })
        .and_then(|live| live.then_some(idx))
    }

    #[cfg(test)]
    pub(crate) fn with_config_write_for_test(&self, f: impl FnOnce()) {
        let _guard = self.config_write.lock().expect("config write poisoned");
        f();
    }

    /// Account `idx`'s `last_selected_seq`, the tick a selection stamps on the account it
    /// picks (`0` = never picked). Test-only: it is how a test proves a request was answered
    /// without any account being SELECTED, which `requests` (a count of SERVED requests)
    /// cannot tell apart from an account picked and then never sent to.
    #[cfg(test)]
    pub(crate) fn last_selected_seq(&self, idx: usize) -> Option<u64> {
        self.accounts
            .read()
            .expect("accounts lock poisoned")
            .get(idx)
            .map(|a| a.last_selected_seq)
    }

    /// Edit account `idx`'s quota in place. Test-only: it is how a test puts an account behind
    /// a window or a refusal without speaking its provider's header dialect, which is a
    /// different parser for each provider.
    #[cfg(test)]
    pub(crate) fn edit_quota_for_test(&self, idx: usize, edit: impl FnOnce(&mut Quota)) {
        if let Some(account) = self
            .accounts
            .write()
            .expect("accounts lock poisoned")
            .get_mut(idx)
        {
            edit(&mut account.quota);
        }
    }

    pub fn proxy_api_key(&self) -> Option<&str> {
        self.proxy_api_key.as_deref()
    }

    /// The request key this server holds, if any ([`crate::request_key`]).
    pub fn request_key(&self) -> Option<&str> {
        self.request_key.as_deref()
    }

    /// The two proxy secrets a client may present, neither of which is a client
    /// identity or a credential for any host but this one.
    pub fn proxy_secrets(&self) -> crate::request_key::ProxySecrets<'_> {
        crate::request_key::ProxySecrets {
            api_key: self.proxy_api_key(),
            request_key: self.request_key(),
        }
    }

    /// `Some(Log)` whenever the server holds a key: this wave refuses nothing.
    pub fn request_key_mode(&self) -> Option<crate::request_key::RequestKeyMode> {
        self.request_key
            .as_ref()
            .map(|_| crate::request_key::RequestKeyMode::Log)
    }

    /// Count one arrival that lacked the request key (log mode: counted, never
    /// refused). See [`crate::request_key::UnkeyedCounters::record`] for the one
    /// INFO line the first of each kind writes.
    pub fn record_unkeyed(
        &self,
        what: crate::request_key::Unkeyed,
        user_agent: crate::request_key::UaFamily,
        peer_port: u16,
    ) {
        self.unkeyed.record(what, user_agent, peer_port);
    }

    /// The `unkeyedConnects` and `unkeyedRequests` rows `GET /_tcr/status`
    /// reports, in that order.
    pub fn unkeyed_wire(
        &self,
    ) -> (
        Vec<crate::status::UnkeyedCount>,
        Vec<crate::status::UnkeyedCount>,
    ) {
        self.unkeyed.wire()
    }

    /// Account `idx`'s OWN upstream-forwarding client — see
    /// [`AccountRuntime::http`] for why every account carries its own. `None`
    /// only if `idx` is stale (accounts are appended, never removed, so this is
    /// never observed on the live serving path — it exists for the same reason
    /// [`Self::access_token`] and [`Self::account_name`] return `Option`).
    pub fn http_client(&self, idx: usize) -> Option<Arc<reqwest::Client>> {
        self.account(idx, |a| Arc::clone(&a.http))
    }

    /// The access token to inject for account `idx` (a clone — the request
    /// outlives the lock).
    pub fn access_token(&self, idx: usize) -> Option<String> {
        self.account(idx, |a| a.access_token.clone())
    }

    /// Whether account `idx` carries a refresh token. `false` for a
    /// `claude setup-token` credential (`tcr login --token`), whose access token
    /// can never be renewed — so for such a row an upstream 401 is proof the
    /// credential is dead, not rotation churn (see the proxy's 401 arm).
    pub fn has_refresh_token(&self, idx: usize) -> bool {
        self.account(idx, |a| a.refresh_token.is_some()) == Some(true)
    }

    /// Display name of account `idx`, for the request log.
    pub fn account_name(&self, idx: usize) -> Option<String> {
        self.account(idx, |a| a.name.clone())
    }

    /// Account `idx`'s exit lock, parsed once out of the config row that
    /// carries it. See [`crate::config::Account::egress_pin`].
    ///
    /// Read from `self.config` and not from the runtime row beside
    /// [`Self::access_token`], because the runtime row holds what this process
    /// mutates per request and a pin is operator intent that only the config
    /// file states. `None` for a stale `idx`, for the same reason
    /// [`Self::access_token`] returns one, and nothing else: a pin that could
    /// not be read never reaches this call, because serde refuses it at config
    /// load rather than handing the request path a value to interpret.
    pub fn account_egress(&self, idx: usize) -> Option<config::EgressPin> {
        self.config
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .egress_pin_at(idx)
    }

    /// The pooled `account_uuid` to inject for account `idx` (a clone — the
    /// request outlives the lock). `None` when the account has no configured
    /// UUID, in which case the proxy leaves the body unchanged.
    pub fn account_uuid(&self, idx: usize) -> Option<String> {
        self.account(idx, |a| a.account_uuid.clone()).flatten()
    }

    /// The ChatGPT account id a Codex row's requests and probes carry as `chatgpt-account-id`:
    /// its workspace, the row's non-empty `org_uuid`, else its `account_uuid`, where a row
    /// saved before the workspace was split from the user keeps that id. `None` for an index
    /// that no longer exists, or a row with neither.
    pub fn codex_account_id(&self, idx: usize) -> Option<String> {
        self.account(idx, |row| {
            row.org_uuid
                .as_deref()
                .filter(|id| !id.is_empty())
                .or(row.account_uuid.as_deref())
                .map(str::to_string)
        })
        .flatten()
    }

    /// A slot's immutable provider, including retired slots. Bound history still
    /// needs this identity to release a conversation through its normal warning path.
    pub fn provider(&self, idx: usize) -> Option<crate::provider::Provider> {
        self.account(idx, |a| a.provider)
    }

    /// A provider only while its slot remains live. Rotation counts must match
    /// `provider_account_count`, which excludes retired rows.
    pub(crate) fn live_provider(&self, idx: usize) -> Option<crate::provider::Provider> {
        self.account(idx, |account| {
            (!account.removed).then_some(account.provider)
        })
        .flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retired_slots_keep_their_provider_identity_but_do_not_count_in_live_rotation() {
        let manager = manager_with(
            r#"{"accounts":[
                {"name":"alice@example.com","accessToken":"at-alice"},
                {"name":"bob@example.com","accessToken":"at-bob","provider":"codex"}
            ]}"#,
        );
        for (idx, provider) in [
            crate::provider::Provider::Anthropic,
            crate::provider::Provider::Codex,
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(manager.provider(idx), Some(provider));
            assert_eq!(manager.live_provider(idx), Some(provider));
            manager.accounts.write().unwrap()[idx].removed = true;
            assert_eq!(
                manager.provider(idx),
                Some(provider),
                "bound history retains a removed slot's provider identity"
            );
            assert_eq!(
                manager.live_provider(idx),
                None,
                "a retired slot cannot consume a live rotation step"
            );
        }
        assert_eq!(manager.provider(2), None);
        assert_eq!(manager.live_provider(2), None);
    }

    fn manager_with(config: &str) -> Arc<Manager> {
        let config: Config = serde_json::from_str(config).expect("the inline test config parses");
        Manager::with_live_refresher(config, None)
    }

    /// The regression guard for the 2026-08-14 incident: a config that lost its
    /// `sessionAffinity` key silently went cold when the default was `false`
    /// (0.952 → 0.498 prompt-cache hit ratio, sustained seven hours). An absent
    /// key must now read as enabled.
    #[test]
    fn session_affinity_defaults_to_enabled_when_key_is_absent() {
        let manager = manager_with(r#"{"accounts": []}"#);
        assert!(
            manager.session_affinity_enabled(),
            "a config with no sessionAffinity key must default to affinity ON"
        );
    }

    /// The explicit opt-out must still work: `"sessionAffinity": false` disables
    /// it.
    #[test]
    fn session_affinity_explicit_false_stays_disabled() {
        let manager = manager_with(r#"{"sessionAffinity": false, "accounts": []}"#);
        assert!(
            !manager.session_affinity_enabled(),
            "an explicit `sessionAffinity: false` must disable affinity"
        );
    }
}
