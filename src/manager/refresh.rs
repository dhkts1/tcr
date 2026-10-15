//! `Manager` OAuth token-refresh methods, split verbatim from `mod.rs`.

use super::*;

impl Manager {
    /// Refresh account `idx`'s token if it is hard-expired OR within the proactive
    /// [`oauth::EXPIRING_SOON_MS`] buffer of expiry, coalescing concurrent callers
    /// into a single upstream refresh. Refreshing ahead of the hard boundary keeps
    /// a batch of in-flight requests from all carrying a just-dead token into an
    /// upstream 401 storm.
    pub async fn ensure_fresh(&self, idx: usize) {
        self.ensure_fresh_inner(idx, false).await;
    }

    /// Force a refresh regardless of expiry (used after a 401), still coalesced.
    /// Returns `true` iff a new token was actually applied — a `false` return means
    /// the force was coalesced, cooldown-suppressed, or the upstream refresh failed,
    /// so the caller (the proxy 401 arm) rotates instead of retrying a dead token.
    pub async fn ensure_fresh_force(&self, idx: usize) -> bool {
        self.ensure_fresh_inner(idx, true).await
    }

    /// Returns `true` iff [`Self::apply_refresh`] ran (a new token was applied).
    ///
    /// The upstream POST runs on a task of its OWN, not inside the caller's future.
    /// A refresh token is single-use: once the POST leaves this box upstream may
    /// rotate it whether or not anyone is still listening. A caller that goes away
    /// mid-refresh — an axum request future dropped because the client hung up — used
    /// to take the in-flight refresh down with it, so the rotated token was lost, the
    /// next attempt re-POSTed a token upstream had already spent, and the `400` that
    /// came back marked a HEALTHY account `Error`, which is terminal. Live proof
    /// (2026-09-16 proxy log): an account logged `refreshing OAuth token` at
    /// 19:07:00.191, never logged an outcome, and its next attempt 57s later was
    /// rejected.
    ///
    /// The caller still awaits the answer; it just no longer owns it. Everything from
    /// the coalescing lock inward moved into [`Self::refresh_locked`] so the lock is
    /// held by the task doing the work, never by a caller that may vanish.
    async fn ensure_fresh_inner(&self, idx: usize, force: bool) -> bool {
        // Decide whether to refresh and snapshot the access token we intend to
        // replace — all before taking the (async) coalescing lock.
        let Some((refresh_token, access_before)) = self.refresh_plan(idx, force, crate::now_ms())
        else {
            return false;
        };

        let lock = match self
            .accounts
            .read()
            .expect("accounts lock poisoned")
            .get(idx)
        {
            Some(account) => account.refresh_lock.clone(),
            None => return false,
        };

        // Detached when we can be: a live `Arc` to ourselves AND a runtime to spawn
        // on. Either missing (a manager built outside `assemble`, a caller on no
        // runtime) falls back to the inline path — same work, same result, just as
        // cancellable as it always was.
        match (
            self.me.get().and_then(std::sync::Weak::upgrade),
            tokio::runtime::Handle::try_current(),
        ) {
            (Some(me), Ok(_)) => tokio::spawn(async move {
                me.refresh_locked(idx, force, refresh_token, access_before, lock)
                    .await
            })
            .await
            .unwrap_or(false),
            _ => {
                self.refresh_locked(idx, force, refresh_token, access_before, lock)
                    .await
            }
        }
    }

    /// The coalescing lock and everything under it. Split out of
    /// [`Self::ensure_fresh_inner`] so it can run on its own task; the lock is
    /// acquired HERE, inside that task, so a cancelled caller cannot release it
    /// while the refresh it started is still in flight.
    async fn refresh_locked(
        &self,
        idx: usize,
        force: bool,
        refresh_token: String,
        access_before: String,
        lock: Arc<tokio::sync::Mutex<()>>,
    ) -> bool {
        let _guard = lock.lock().await;

        // Coalesce ALL concurrent callers, the force path included (bug #10): a
        // leader that already refreshed while we queued on the lock changed the
        // access token, so a follower sees a different token here and skips the
        // upstream refresh entirely — and so never sends the now-rotated stale
        // refresh token (which would 401 and wrongly sideline a healthy account).
        if self.access_token(idx).as_deref() != Some(access_before.as_str()) {
            return false;
        }
        // A *transient* leader failure leaves the access token unchanged, so the
        // guard above can't catch us — but the leader stamped a short cooldown
        // before releasing the lock. Honor it: skip re-POSTing the same
        // single-use refresh token while the cooldown is still live. It
        // self-clears by time, so a later sequential retry is not suppressed.
        {
            let accounts = self.accounts.read().expect("accounts lock poisoned");
            if let Some(until) = accounts.get(idx).and_then(|a| a.refresh_retry_after_ms) {
                if crate::now_ms() < until {
                    return false;
                }
            }
        }
        let (name, account_uuid, org_uuid, org_name, provider, codex_home) = {
            let accounts = self.accounts.read().expect("accounts lock poisoned");
            match accounts.get(idx) {
                Some(a) => (
                    a.name.clone(),
                    a.account_uuid.clone(),
                    a.org_uuid.clone(),
                    a.org_name.clone(),
                    a.provider,
                    a.codex_home.clone(),
                ),
                None => return false,
            }
        };

        let imported_home = match provider {
            crate::provider::Provider::Anthropic => None,
            crate::provider::Provider::Codex => codex_home.map(std::path::PathBuf::from),
        };
        let file_before = match imported_home.as_deref() {
            Some(home) => crate::codex_token_store::read_async(home).await.ok(),
            None => None,
        };
        let loaded_refresh_ms = file_before
            .as_ref()
            .filter(|auth| auth.login.refresh_token == refresh_token)
            .and_then(|auth| auth.last_refresh_ms);
        let newer_in_file = |after_ms| {
            let home = imported_home.clone();
            let refresh_token = refresh_token.clone();
            let access_before = access_before.clone();
            let org_uuid = org_uuid.clone();
            let account_uuid = account_uuid.clone();
            async move {
                let home = home?;
                crate::codex_token_store::read_async(&home)
                    .await
                    .ok()
                    .and_then(|auth| {
                        auth.newer_tokens(
                            &refresh_token,
                            &access_before,
                            org_uuid.as_deref(),
                            account_uuid.as_deref(),
                            after_ms,
                        )
                    })
            }
        };
        // Codex may have refreshed since this row was loaded. Adopt its live pair before
        // spending the stale single-use refresh token, and check again after an upstream race.
        let preexisting = newer_in_file(None).await;
        let result = if let Some(tokens) = preexisting {
            Ok(tokens)
        } else {
            tracing::info!(account = %name, "refreshing OAuth token");
            match self
                .refresher
                .refresh(provider, refresh_token.clone())
                .await
            {
                Ok(mut tokens) => {
                    if let Some(home) = imported_home.as_deref() {
                        match crate::codex_token_store::write_back_async(
                            home,
                            &refresh_token,
                            &tokens,
                        )
                        .await
                        {
                            Ok(true) => {}
                            Ok(false) => {
                                if let Some(newer) = newer_in_file(loaded_refresh_ms).await {
                                    tokens = newer;
                                }
                            }
                            Err(error) => tracing::warn!(
                                account = %name,
                                error_kind = ?error.kind(),
                                "could not synchronize refreshed tokens with the imported Codex login"
                            ),
                        }
                    }
                    Ok(tokens)
                }
                // A concurrent Codex rotation can change the file during any failed POST,
                // not only one that names a reused token. A different live pair for the same
                // account is stronger evidence than this stale token's failure.
                Err(error) => newer_in_file(loaded_refresh_ms).await.ok_or(error),
            }
        };
        match result {
            Ok(tokens) => {
                // A *forced* success re-arms the refresh cooldown, collapsing a herd
                // of concurrent 401-driven force-refreshes to ONE rotation instead of
                // N back-to-back. Passing the deadline INTO apply_refresh folds the
                // re-arm and the success-path clear into one write, so the throttle
                // can never be clobbered by an ordering slip between two lock
                // sections — the policy (whether/what) stays here, the single atomic
                // write lives in apply_refresh.
                let cooldown_after = force.then(Self::cooldown_deadline);
                self.apply_refresh(idx, &tokens, cooldown_after);
                if let Some(manager) = self.me.get().and_then(std::sync::Weak::upgrade) {
                    let persisted_name = name.clone();
                    let persisted_tokens = tokens.clone();
                    if tokio::task::spawn_blocking(move || {
                        manager.persist_tokens(
                            &persisted_name,
                            account_uuid,
                            org_uuid,
                            org_name,
                            provider,
                            &persisted_tokens,
                        );
                    })
                    .await
                    .is_err()
                    {
                        tracing::error!(account = %name, "refreshed token persistence task failed");
                    }
                } else {
                    // Only manually constructed managers lack the Arc installed by assemble;
                    // retain their existing inline refresh/persistence fallback.
                    self.persist_tokens(&name, account_uuid, org_uuid, org_name, provider, &tokens);
                }
                tracing::info!(account = %name, "token refreshed");
                true
            }
            Err(oauth::OAuthError::AuthRejected { status, code }) => {
                tracing::error!(account = %name, status, "refresh token rejected — re-login needed");
                if let Some(a) = self
                    .accounts
                    .write()
                    .expect("accounts lock poisoned")
                    .get_mut(idx)
                {
                    a.status = AccountStatus::Error;
                    // Why a Codex login is dead, for the status wire: the OAuth code when the
                    // rejection named one, else the status it came back with. An Anthropic row
                    // keeps none, so its wire row gains no key.
                    match a.provider {
                        crate::provider::Provider::Anthropic => {}
                        crate::provider::Provider::Codex => {
                            a.login_error =
                                Some(code.map_or_else(|| format!("http_{status}"), str::to_string));
                        }
                    }
                }
                // A REJECTED refresh is the one signal that this credential may truly
                // be dead — back off before the next recovery attempt (first rejection
                // arms the base delay; each further one doubles it). A *transient*
                // failure deliberately does NOT back off: it proves nothing, so the
                // next probe tick may retry promptly.
                self.grow_error_backoff(idx);
                false
            }
            Err(oauth::OAuthError::Transient(msg)) => {
                // Keep the current token; this one request fails over instead.
                // The token is UNCHANGED, so the access-token guard can't stop a
                // follower already queued on the lock from re-POSTing the same
                // single-use refresh token. Stamp a short self-clearing cooldown
                // (still under this lock, so queued followers see it) to coalesce
                // the concurrent batch; a later sequential retry still refreshes.
                if let Some(a) = self
                    .accounts
                    .write()
                    .expect("accounts lock poisoned")
                    .get_mut(idx)
                {
                    a.refresh_retry_after_ms = Some(Self::cooldown_deadline());
                }
                tracing::warn!(account = %name, error = %msg, "token refresh transient failure");
                false
            }
        }
    }

    /// If account `idx` should be refreshed now, return `(refresh_token,
    /// access_before)` — the refresh token to send and the access token we mean
    /// to replace. `access_before` lets [`Self::ensure_fresh_inner`] detect, under
    /// the coalescing guard, that another caller already refreshed (the access
    /// token changed), coalescing even the force path (bug #10).
    pub(super) fn refresh_plan(
        &self,
        idx: usize,
        force: bool,
        now_ms: i64,
    ) -> Option<(String, String)> {
        // A refresh token is single-use. Once this process has handed its
        // listening socket to a successor, the successor holds the same tokens
        // and is the one entitled to rotate them: rotating here would invalidate
        // its copy, which is the token war `crate::singleton` exists to prevent
        // arriving from inside one lineage instead of between two rival proxies.
        //
        // Gated at `refresh_plan` rather than in `ensure_fresh_inner` because
        // this is the single point where the decision to refresh is made, and
        // `ensure_fresh_inner` already returns early on `None`. One gate, not
        // one per caller.
        if self.mutation_is_released() {
            return None;
        }
        let accounts = self.accounts.read().expect("accounts lock poisoned");
        let account = accounts.get(idx)?;
        // Deleted from the config: nothing will ever store a rotated token for
        // it, so rotating would only burn the single-use refresh token.
        if !account.account_type.is_oauth() || account.removed {
            return None;
        }
        let refresh_token = account.refresh_token.clone()?;
        let is_expired = oauth::is_expired(account.expires_at_ms, now_ms);
        // Proactive refresh: a token within the EXPIRING_SOON buffer is refreshed
        // before it hard-expires, so a batch of in-flight requests at the boundary
        // never all carry a just-dead token into an upstream 401 storm.
        let is_expiring_soon = oauth::is_expiring_soon(account.expires_at_ms, now_ms);
        if !(force || is_expired || is_expiring_soon) {
            return None;
        }
        // A transient failure stamped a short cooldown to coalesce the batch —
        // don't START a new refresh during it. A hard-expired token under
        // `force` still overrides, so a dead-in-hand token is never pinned
        // un-refreshable by the cooldown (the cooldown also self-clears by time).
        let in_cooldown = account
            .refresh_retry_after_ms
            .is_some_and(|until| now_ms < until);
        if in_cooldown && !(force && is_expired) {
            return None;
        }
        Some((refresh_token, account.access_token.clone()))
    }

    /// The wall-clock deadline before which no new refresh of an account may
    /// START. Single source of the cooldown formula so the transient-failure arm
    /// and the forced-success re-arm can never drift apart.
    fn cooldown_deadline() -> i64 {
        crate::now_ms() + REFRESH_RETRY_COOLDOWN_MS
    }

    /// Install a freshly refreshed token pair. `cooldown_after` is the refresh
    /// floor set atomically with the install: `None` clears any transient-failure
    /// cooldown (a good refresh ends it); `Some(deadline)` re-arms it for a
    /// *forced* success so a herd of concurrent 401 force-refreshes collapses to
    /// one rotation. Folding it in here makes the clear-vs-re-arm a single write —
    /// no ordering hazard between two separate lock sections.
    fn apply_refresh(&self, idx: usize, tokens: &Tokens, cooldown_after: Option<i64>) {
        let mut accounts = self.accounts.write().expect("accounts lock poisoned");
        if let Some(account) = accounts.get_mut(idx) {
            account.access_token = tokens.access_token.clone();
            // THE TRAP: `tokens.refresh_token` must be `Some(...)` on every
            // reachable refresh success — see [`Tokens::refresh_token`]'s doc
            // comment. Writing a bare `None` here would silently make a
            // healthy account unrefreshable with no error and no log line.
            account.refresh_token = tokens.refresh_token.clone();
            account.expires_at_ms = Some(tokens.expires_at_ms);
            account.refresh_retry_after_ms = cooldown_after;
            if account.status == AccountStatus::Error {
                // RECOVERY: a refresh just succeeded, which a genuinely dead
                // credential could never do — so this row was never dead, and it is
                // now holding a fresh valid token. Clear the error backoff with it.
                account.status = AccountStatus::Active;
            }
            account.error_retry_after_ms = None;
            account.error_backoff_ms = 0;
            // A login that just refreshed is not dead, whatever the last rejection said.
            account.login_error = None;
        }
    }

    /// Arm (or double) the recovery backoff for an errored row. First rejection
    /// arms [`ERROR_REPROBE_BASE_MS`]; each subsequent one doubles up to
    /// [`ERROR_REPROBE_CAP_MS`], so a permanently dead credential is retried at most
    /// ~twice an hour instead of hammering the OAuth endpoint (which would risk
    /// rate-limiting the whole fleet).
    pub(super) fn grow_error_backoff(&self, idx: usize) {
        let mut accounts = self.accounts.write().expect("accounts lock poisoned");
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    const ACCOUNT_ID: &str = "11111111-1111-4111-8111-111111111111";
    const USER_ID: &str = "22222222-2222-4222-8222-222222222222";

    fn access_token(account_id: &str, issued_at: i64, expires_at: i64) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let payload = serde_json::to_vec(&json!({
            "iat": issued_at,
            "exp": expires_at,
            "https://api.openai.com/auth": {
                "chatgpt_account_id": account_id,
                "chatgpt_user_id": USER_ID,
            }
        }))
        .unwrap();
        let mut encoded = String::new();
        for chunk in payload.chunks(3) {
            let bits = (u32::from(chunk[0]) << 16)
                | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
                | u32::from(*chunk.get(2).unwrap_or(&0));
            for shift in [18, 12, 6, 0].into_iter().take(chunk.len() + 1) {
                encoded.push(ALPHABET[((bits >> shift) & 63) as usize] as char);
            }
        }
        format!("e30.{encoded}.sig")
    }

    fn auth_document(refresh: &str, issued_at: i64) -> Value {
        json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": "id-token-kept",
                "access_token": access_token(ACCOUNT_ID, issued_at, 4_102_444_800),
                "refresh_token": refresh,
                "account_id": ACCOUNT_ID,
                "future_token_field": {"keep": true},
            },
            "last_refresh": if issued_at == 1 { "2026-10-01T00:00:00Z" } else { "2090-01-01T00:00:00Z" },
            "future_top_level": ["keep", 1],
        })
    }

    fn write_auth(home: &Path, document: &Value) {
        std::fs::write(home.join("auth.json"), document.to_string()).unwrap();
    }

    fn read_auth(home: &Path) -> Value {
        serde_json::from_slice(&std::fs::read(home.join("auth.json")).unwrap()).unwrap()
    }

    struct TestRefresh {
        home: PathBuf,
        replacement: Option<Value>,
        failure_code: Option<&'static str>,
        calls: Arc<AtomicUsize>,
    }

    impl TokenRefresher for TestRefresh {
        fn refresh(
            &self,
            provider: crate::provider::Provider,
            token: String,
        ) -> oauth::RefreshFuture {
            assert_eq!(provider, crate::provider::Provider::Codex);
            assert_eq!(token, "rt-old");
            let home = self.home.clone();
            let replacement = self.replacement.clone();
            let failure_code = self.failure_code;
            let calls = Arc::clone(&self.calls);
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                if let Some(document) = replacement {
                    write_auth(&home, &document);
                }
                if failure_code == Some("transient") {
                    Err(oauth::OAuthError::Transient(
                        "temporary transport failure".to_string(),
                    ))
                } else if let Some(code) = failure_code {
                    Err(oauth::OAuthError::AuthRejected {
                        status: 401,
                        code: Some(code),
                    })
                } else {
                    Ok(Tokens {
                        id_token: None,
                        access_token: "at-tcr-new".to_string(),
                        refresh_token: Some("rt-tcr-new".to_string()),
                        expires_at_ms: crate::now_ms() + 3_600_000,
                    })
                }
            })
        }
    }

    struct NoNetwork;
    impl UsageProber for NoNetwork {
        fn probe(&self, _: crate::probe::ProbeCredential) -> crate::probe::ProbeFuture {
            panic!("refresh tests never probe");
        }
    }
    impl AccountWarmer for NoNetwork {
        fn warm(&self, _: String, _: String) -> crate::warmer::WarmFuture {
            panic!("refresh tests never warm");
        }
    }

    fn imported_manager(
        home: &Path,
        replacement: Option<Value>,
        reused: bool,
    ) -> (Arc<Manager>, Arc<AtomicUsize>) {
        imported_manager_with_failure(home, replacement, reused.then_some("refresh_token_reused"))
    }

    fn imported_manager_with_failure(
        home: &Path,
        replacement: Option<Value>,
        failure_code: Option<&'static str>,
    ) -> (Arc<Manager>, Arc<AtomicUsize>) {
        let config: Config = serde_json::from_value(json!({"accounts": [{
            "name": "alice@example.com/codex",
            "provider": "codex",
            "accountUuid": USER_ID,
            "orgUuid": ACCOUNT_ID,
            "accessToken": access_token(ACCOUNT_ID, 1, 4_102_444_800),
            "refreshToken": "rt-old",
            "expiresAt": 0,
            "codexHome": home,
        }]}))
        .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let refresher = TestRefresh {
            home: home.to_path_buf(),
            replacement,
            failure_code,
            calls: Arc::clone(&calls),
        };
        (
            Manager::new(
                config,
                Arc::new(refresher),
                Arc::new(NoNetwork),
                Arc::new(NoNetwork),
                None,
            ),
            calls,
        )
    }

    #[tokio::test]
    async fn an_imported_codex_refresh_writes_the_rotated_tokens_back() {
        let home = tempfile::tempdir().unwrap();
        write_auth(home.path(), &auth_document("rt-old", 1));
        let (manager, calls) = imported_manager(home.path(), None, false);
        assert!(manager.ensure_fresh_force(0).await);
        let document = read_auth(home.path());
        assert_eq!(document["tokens"]["refresh_token"], "rt-tcr-new");
        assert_eq!(document["tokens"]["access_token"], "at-tcr-new");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn an_imported_codex_writeback_preserves_every_other_json_field() {
        let home = tempfile::tempdir().unwrap();
        let original = auth_document("rt-old", 1);
        write_auth(home.path(), &original);
        let (manager, _) = imported_manager(home.path(), None, false);
        assert!(manager.ensure_fresh_force(0).await);
        let mut expected = original;
        expected["tokens"]["access_token"] = json!("at-tcr-new");
        expected["tokens"]["refresh_token"] = json!("rt-tcr-new");
        let document = read_auth(home.path());
        assert_ne!(document["last_refresh"], expected["last_refresh"]);
        expected["last_refresh"] = document["last_refresh"].clone();
        assert_eq!(document, expected);
    }

    #[tokio::test]
    async fn an_imported_codex_writeback_is_private_and_replaces_the_file_atomically() {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("auth.json");
        write_auth(home.path(), &auth_document("rt-old", 1));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let before_inode = std::fs::metadata(&path).unwrap().ino();
        let (manager, _) = imported_manager(home.path(), None, false);
        assert!(manager.ensure_fresh_force(0).await);
        let after = std::fs::metadata(&path).unwrap();
        assert_eq!(after.permissions().mode() & 0o777, 0o600);
        assert_ne!(
            after.ino(),
            before_inode,
            "a same-directory rename publishes a new inode"
        );
    }

    #[tokio::test]
    async fn a_newer_imported_codex_login_is_adopted_before_refreshing() {
        let home = tempfile::tempdir().unwrap();
        let newer = auth_document("rt-codex-new", 2);
        write_auth(home.path(), &newer);
        let (manager, calls) = imported_manager(home.path(), None, false);
        assert!(manager.ensure_fresh_force(0).await);
        assert_eq!(
            manager.access_token(0).unwrap(),
            newer["tokens"]["access_token"]
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "do not spend the stale refresh token"
        );
        assert_eq!(read_auth(home.path()), newer);
    }

    #[tokio::test]
    async fn a_reused_imported_codex_refresh_recovers_the_newer_file_login() {
        let home = tempfile::tempdir().unwrap();
        write_auth(home.path(), &auth_document("rt-old", 1));
        let newer = auth_document("rt-codex-new", 2);
        let (manager, calls) = imported_manager(home.path(), Some(newer.clone()), true);
        assert!(
            manager.ensure_fresh_force(0).await,
            "the newer file login recovers the row"
        );
        assert_eq!(
            manager.access_token(0).unwrap(),
            newer["tokens"]["access_token"]
        );
        assert_eq!(manager.account_status(0), Some(AccountStatus::Active));
        assert_eq!(
            manager.snapshot(OffsetDateTime::now_utc()).accounts[0]
                .codex
                .login_error,
            None
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(read_auth(home.path()), newer);
    }

    #[tokio::test]
    async fn an_imported_codex_file_changed_during_refresh_is_not_overwritten() {
        let home = tempfile::tempdir().unwrap();
        write_auth(home.path(), &auth_document("rt-old", 1));
        let newer = auth_document("rt-codex-new", 2);
        let (manager, _) = imported_manager(home.path(), Some(newer.clone()), false);
        assert!(manager.ensure_fresh_force(0).await);
        assert_eq!(
            read_auth(home.path()),
            newer,
            "a changed refresh token guards the file"
        );
        assert_eq!(
            manager.access_token(0).unwrap(),
            newer["tokens"]["access_token"]
        );
    }

    #[tokio::test]
    async fn a_reused_imported_codex_refresh_rejects_foreign_expired_or_older_file_tokens() {
        let mut foreign = auth_document("rt-other", 2);
        foreign["tokens"]["account_id"] = json!("33333333-3333-4333-8333-333333333333");
        foreign["tokens"]["access_token"] = json!(access_token(
            "33333333-3333-4333-8333-333333333333",
            2,
            4_102_444_800,
        ));
        let mut expired = auth_document("rt-expired", 2);
        expired["tokens"]["access_token"] = json!(access_token(ACCOUNT_ID, 2, 1));
        let mut older = auth_document("rt-older", 2);
        older["last_refresh"] = json!("2026-09-01T00:00:00Z");
        for document in [foreign, expired, older, auth_document("rt-old", 2)] {
            let home = tempfile::tempdir().unwrap();
            write_auth(home.path(), &auth_document("rt-old", 1));
            let (manager, _) = imported_manager(home.path(), Some(document.clone()), true);
            assert!(!manager.ensure_fresh_force(0).await);
            assert_eq!(manager.account_status(0), Some(AccountStatus::Error));
            assert_eq!(read_auth(home.path()), document);
        }
    }

    #[tokio::test]
    async fn a_changed_imported_codex_login_is_adopted_after_any_refresh_failure() {
        let mut failures = Vec::new();
        for code in [
            "refresh_token_expired",
            "refresh_token_invalidated",
            "transient",
        ] {
            let home = tempfile::tempdir().unwrap();
            write_auth(home.path(), &auth_document("rt-old", 1));
            let newer = auth_document("rt-codex-new", 2);
            let (manager, _) =
                imported_manager_with_failure(home.path(), Some(newer.clone()), Some(code));
            if !manager.ensure_fresh_force(0).await {
                failures.push(code);
                continue;
            }
            assert_eq!(
                manager.access_token(0).unwrap(),
                newer["tokens"]["access_token"]
            );
            assert_eq!(manager.account_status(0), Some(AccountStatus::Active));
            assert_eq!(read_auth(home.path()), newer);
        }
        assert!(
            failures.is_empty(),
            "newer file login was not adopted after: {failures:?}"
        );
    }

    #[tokio::test]
    async fn a_successful_imported_codex_refresh_leaves_a_different_account_file_untouched() {
        let home = tempfile::tempdir().unwrap();
        write_auth(home.path(), &auth_document("rt-old", 1));
        let mut other = auth_document("rt-other", 2);
        other["tokens"]["account_id"] = json!("33333333-3333-4333-8333-333333333333");
        let (manager, _) = imported_manager(home.path(), Some(other.clone()), false);
        assert!(manager.ensure_fresh_force(0).await);
        assert_eq!(manager.access_token(0).as_deref(), Some("at-tcr-new"));
        assert_eq!(read_auth(home.path()), other);
    }

    #[tokio::test]
    async fn an_imported_codex_refresh_never_overwrites_an_unreadable_auth_file() {
        use std::os::unix::fs::PermissionsExt as _;
        let home = tempfile::tempdir().unwrap();
        let original = auth_document("rt-old", 1);
        write_auth(home.path(), &original);
        let path = home.path().join("auth.json");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let (manager, _) = imported_manager(home.path(), None, false);
        assert!(manager.ensure_fresh_force(0).await);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(mode, 0o000, "an unreadable login is never replaced");
        assert_eq!(read_auth(home.path()), original);
    }

    #[tokio::test]
    async fn a_failed_imported_codex_writeback_never_rolls_back_live_tokens() {
        use std::os::unix::fs::PermissionsExt as _;
        use std::sync::Mutex;

        struct RotatingRefresh(Arc<Mutex<Vec<String>>>);
        impl TokenRefresher for RotatingRefresh {
            fn refresh(&self, _: crate::provider::Provider, token: String) -> oauth::RefreshFuture {
                let mut calls = self.0.lock().unwrap();
                calls.push(token);
                let generation = calls.len();
                Box::pin(async move {
                    Ok(Tokens {
                        id_token: None,
                        access_token: access_token(
                            ACCOUNT_ID,
                            generation as i64 + 1,
                            4_102_444_800,
                        ),
                        refresh_token: Some(format!("rt-tcr-{generation}")),
                        expires_at_ms: crate::now_ms() + 3_600_000,
                    })
                })
            }
        }

        let mut rollbacks = Vec::new();
        for restart in [false, true] {
            let home = tempfile::tempdir().unwrap();
            let original = auth_document("rt-old", 1);
            write_auth(home.path(), &original);
            std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
            let calls = Arc::new(Mutex::new(Vec::new()));
            let make_manager = |access: String, refresh: &str| {
                let config: Config = serde_json::from_value(json!({"accounts": [{
                    "name": "alice@example.com/codex", "provider": "codex",
                    "accountUuid": USER_ID, "orgUuid": ACCOUNT_ID,
                    "accessToken": access, "refreshToken": refresh, "expiresAt": 0,
                    "codexHome": home.path(),
                }]}))
                .unwrap();
                Manager::new(
                    config,
                    Arc::new(RotatingRefresh(Arc::clone(&calls))),
                    Arc::new(NoNetwork),
                    Arc::new(NoNetwork),
                    None,
                )
            };
            let mut manager = make_manager(access_token(ACCOUNT_ID, 1, 4_102_444_800), "rt-old");
            assert!(manager.ensure_fresh_force(0).await);
            assert_eq!(
                read_auth(home.path()),
                original,
                "the writeback must have failed"
            );
            if restart {
                manager = make_manager(manager.access_token(0).unwrap(), "rt-tcr-1");
            } else {
                let mut accounts = manager.accounts.write().unwrap();
                accounts[0].refresh_retry_after_ms = None;
                accounts[0].expires_at_ms = Some(0);
            }
            assert!(manager.ensure_fresh_force(0).await);
            let calls = calls.lock().unwrap();
            if calls.as_slice() != ["rt-old", "rt-tcr-1"] {
                rollbacks.push((restart, calls.clone()));
            }
            assert_eq!(read_auth(home.path()), original);
            std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        assert!(
            rollbacks.is_empty(),
            "stale file login replaced newer runtime tokens: {rollbacks:?}"
        );
    }

    #[tokio::test]
    async fn imported_codex_file_io_keeps_the_async_runtime_responsive() {
        use std::io::Write as _;
        let home = tempfile::tempdir().unwrap();
        let auth_path = home.path().join("auth.json");
        assert!(std::process::Command::new("/usr/bin/mkfifo")
            .arg(&auth_path)
            .status()
            .unwrap()
            .success());
        let (manager, _) = imported_manager(home.path(), None, false);
        let writer = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(500));
            let mut fifo = std::fs::OpenOptions::new()
                .write(true)
                .open(&auth_path)
                .unwrap();
            // The first read holds the FIFO inode; later reads see a normal fixture file.
            std::fs::remove_file(&auth_path).unwrap();
            let body = auth_document("rt-old", 1).to_string();
            std::fs::write(&auth_path, &body).unwrap();
            fifo.write_all(body.as_bytes()).unwrap();
        });
        let started = std::time::Instant::now();
        let refresh = tokio::spawn(async move { manager.ensure_fresh_force(0).await });
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        let heartbeat = started.elapsed();
        assert!(refresh.await.unwrap());
        writer.join().unwrap();
        assert!(
            heartbeat < std::time::Duration::from_millis(250),
            "blocking auth.json read stalled the runtime for {heartbeat:?}"
        );
    }

    #[tokio::test]
    async fn refreshed_token_config_persistence_keeps_the_async_runtime_responsive() {
        let home = tempfile::tempdir().unwrap();
        let (original, _) = imported_manager(home.path(), None, false);
        let mut config = original.config.lock().unwrap().clone();
        config.accounts[0].codex_home = None;
        let path = home.path().join(crate::paths::CONFIG_FILE);
        std::fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
        let manager = Manager::new(
            config,
            Arc::clone(&original.refresher),
            Arc::new(NoNetwork),
            Arc::new(NoNetwork),
            Some(path),
        );
        let lock = Arc::clone(&manager.config_write);
        let (ready, acquired) = std::sync::mpsc::channel();
        let writer = std::thread::spawn(move || {
            let _guard = lock.lock().unwrap();
            ready.send(()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(500));
        });
        acquired.recv().unwrap();
        let started = std::time::Instant::now();
        let refresh = tokio::spawn(async move { manager.ensure_fresh_force(0).await });
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        let heartbeat = started.elapsed();
        assert!(refresh.await.unwrap());
        writer.join().unwrap();
        assert!(
            heartbeat < std::time::Duration::from_millis(250),
            "config token persistence stalled the runtime for {heartbeat:?}"
        );
    }
}
