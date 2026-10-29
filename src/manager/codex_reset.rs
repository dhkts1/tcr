//! Confirmed named-account redemption. Probes share the row's gate but never call consume.
use super::*;
use crate::provider::Provider;
use std::time::Duration;
use tokio::time::{Instant, timeout_at};

const CONSUME_PATH: &str = "/backend-api/wham/rate-limit-reset-credits/consume";

/// One recognized backend outcome; no backend identity is exposed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CodexResetOutcome {
    Applied,
    NothingToReset,
    NoCredit,
    AlreadyRedeemed,
}

impl CodexResetOutcome {
    pub const fn message(self) -> &'static str {
        match self {
            Self::Applied => "Applied.",
            Self::NothingToReset => "Nothing to reset.",
            Self::NoCredit => "No reset credit available.",
            Self::AlreadyRedeemed => "Already redeemed.",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodexResetResponse {
    pub outcome: CodexResetOutcome,
    pub usage_refreshed: bool,
}

/// Fixed reasons only: transport errors and backend bodies may contain credentials.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodexResetError {
    Missing,
    Ambiguous,
    WrongProvider,
    Unavailable,
    Failed,
    Unknown,
}

impl CodexResetError {
    pub const fn message(self) -> &'static str {
        match self {
            Self::Missing => "The named account is not listed.",
            Self::Ambiguous => "The account name is ambiguous.",
            Self::WrongProvider => "The named account must use `codex`.",
            Self::Unavailable => "The account or mutation owner changed.",
            Self::Failed => "The reset request was refused.",
            Self::Unknown => "Reset outcome unknown.",
        }
    }
}

/// A snapshot for rechecking identity after awaits; never formatted or logged.
#[derive(Clone, PartialEq, Eq)]
pub(super) struct BoundAccount {
    name: String,
    provider: Provider,
    account: Option<String>,
    org: Option<String>,
    org_name: Option<String>,
}

impl BoundAccount {
    fn of(row: &AccountRuntime) -> Self {
        Self {
            name: row.name.clone(),
            provider: row.provider,
            account: row.account_uuid.clone(),
            org: row.org_uuid.clone(),
            org_name: row.org_name.clone(),
        }
    }

    pub(super) fn at(manager: &Manager, idx: usize) -> Option<Self> {
        manager
            .accounts
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(idx)
            .filter(|row| !row.removed)
            .map(Self::of)
    }

    pub(super) fn is_current(&self, manager: &Manager, idx: usize) -> bool {
        Self::at(manager, idx).as_ref() == Some(self)
    }
}

/// The accepted local key alphabet, with a small fixed bound.
pub fn valid_reset_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 128
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

#[derive(serde::Deserialize)]
struct ConsumeResponse {
    code: ConsumeCode,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum ConsumeCode {
    Reset,
    NothingToReset,
    NoCredit,
    AlreadyRedeemed,
}

impl From<ConsumeCode> for CodexResetOutcome {
    fn from(code: ConsumeCode) -> Self {
        match code {
            ConsumeCode::Reset => Self::Applied,
            ConsumeCode::NothingToReset => Self::NothingToReset,
            ConsumeCode::NoCredit => Self::NoCredit,
            ConsumeCode::AlreadyRedeemed => Self::AlreadyRedeemed,
        }
    }
}

impl Manager {
    pub(super) fn usage_gate(&self, idx: usize) -> Option<Arc<AsyncMutex<()>>> {
        BoundAccount::at(self, idx)?;
        let mut gates = self
            .usage_gates
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        Some(Arc::clone(
            gates
                .entry(idx)
                .or_insert_with(|| Arc::new(AsyncMutex::new(()))),
        ))
    }

    /// The only path that consumes. The caller must have confirmed operator input.
    /// Queueing, refresh and consume share one deadline. A known result survives a
    /// post-read timeout, and the gate stays held until that read finishes or expires.
    pub async fn reset_codex(
        &self,
        query: &str,
        key: &str,
    ) -> Result<CodexResetResponse, CodexResetError> {
        if !valid_reset_key(key) {
            return Err(CodexResetError::Failed);
        }
        let deadline = Instant::now() + Duration::from_secs(90);
        let (idx, identity, guard, outcome) = timeout_at(deadline, async {
            let (idx, identity) = {
                let accounts = self.accounts.read().unwrap_or_else(PoisonError::into_inner);
                let idx = match match_listed(&accounts, query) {
                    crate::identity::Match::None => return Err(CodexResetError::Missing),
                    crate::identity::Match::Ambiguous(_) => return Err(CodexResetError::Ambiguous),
                    crate::identity::Match::One(idx) => idx,
                };
                let row = &accounts[idx];
                if row.provider != Provider::Codex {
                    return Err(CodexResetError::WrongProvider);
                }
                (idx, BoundAccount::of(row))
            };
            let gate = self.usage_gate(idx).ok_or(CodexResetError::Unavailable)?;
            let guard = gate.lock_owned().await;
            self.check_reset_owner(idx, &identity)?;
            self.ensure_fresh(idx).await;
            self.check_reset_owner(idx, &identity)?;
            let client = crate::http_client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(10))
                .build()
                .map_err(|_| CodexResetError::Failed)?;
            let url = format!(
                "{}{CONSUME_PATH}",
                self.upstream_for(Provider::Codex).trim_end_matches('/')
            );
            let body = serde_json::json!({"redeem_request_id":key});
            for attempt in 0..2 {
                self.check_reset_owner(idx, &identity)?;
                let credential = self
                    .access_token(idx)
                    .and_then(|token| self.probe_credential(idx, token))
                    .ok_or(CodexResetError::Unavailable)?;
                self.check_reset_owner(idx, &identity)?;
                let mut request = client
                    .post(&url)
                    .bearer_auth(credential.access_token)
                    .header(
                        reqwest::header::USER_AGENT,
                        concat!("tcr/", env!("CARGO_PKG_VERSION")),
                    )
                    .json(&body);
                if let Some(id) = credential.account_id {
                    request = request.header(crate::provider::CODEX_ACCOUNT_HEADER, id);
                }
                let response = request.send().await.map_err(|_| CodexResetError::Unknown)?;
                if response.status() == reqwest::StatusCode::UNAUTHORIZED && attempt == 0 {
                    self.check_reset_owner(idx, &identity)?;
                    if !self.ensure_fresh_force(idx).await {
                        return Err(CodexResetError::Failed);
                    }
                    self.check_reset_owner(idx, &identity)?;
                    continue;
                }
                if response.status().is_server_error() {
                    return Err(CodexResetError::Unknown);
                }
                if !response.status().is_success() {
                    return Err(CodexResetError::Failed);
                }
                let response: ConsumeResponse = response
                    .json()
                    .await
                    .map_err(|_| CodexResetError::Unknown)?;
                return Ok((idx, identity, guard, CodexResetOutcome::from(response.code)));
            }
            Err(CodexResetError::Failed)
        })
        .await
        .map_err(|_| CodexResetError::Unknown)??;
        let usage_refreshed = if identity.is_current(self, idx) && !self.mutation_is_released() {
            timeout_at(deadline, self.probe_account_unlocked(idx))
                .await
                .unwrap_or(false)
                && identity.is_current(self, idx)
        } else {
            false
        };
        drop(guard);
        Ok(CodexResetResponse {
            outcome,
            usage_refreshed,
        })
    }

    fn check_reset_owner(
        &self,
        idx: usize,
        identity: &BoundAccount,
    ) -> Result<(), CodexResetError> {
        if self.mutation_is_released() || !identity.is_current(self, idx) {
            Err(CodexResetError::Unavailable)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Account;
    use crate::test_support::stub_upstream as stub;
    use axum::body::to_bytes;
    use axum::extract::Request;
    use axum::http::StatusCode;
    use axum::response::{IntoResponse, Response};
    use std::sync::atomic::AtomicUsize;

    #[derive(Clone)]
    struct Seen {
        body: serde_json::Value,
        auth: Option<String>,
        account: Option<String>,
        agent_ok: bool,
    }

    struct Backend {
        url: String,
        consumes: AtomicUsize,
        debits: AtomicUsize,
        reads: AtomicUsize,
        seen: Mutex<Vec<Seen>>,
        redeemed: Mutex<HashSet<String>>,
        code: &'static str,
        status: StatusCode,
        first_401: AtomicBool,
        lose_first: AtomicBool,
        fail_read: AtomicBool,
        block_read: AtomicBool,
        read_started: Notify,
        release_read: Notify,
        delay_consume: AtomicBool,
        other: AtomicUsize,
    }

    impl Backend {
        fn new(code: &'static str, status: StatusCode) -> Arc<Self> {
            Arc::new_cyclic(|weak: &std::sync::Weak<Self>| {
                let weak = std::sync::Weak::clone(weak);
                let origin = stub::http(axum::Router::new().fallback(move |req: Request| {
                    let backend = weak.upgrade().unwrap();
                    async move { backend.respond(req).await }
                }));
                Self {
                    url: origin.url(),
                    consumes: AtomicUsize::new(0),
                    debits: AtomicUsize::new(0),
                    reads: AtomicUsize::new(0),
                    seen: Mutex::new(Vec::new()),
                    redeemed: Mutex::new(HashSet::new()),
                    code,
                    status,
                    first_401: AtomicBool::new(false),
                    lose_first: AtomicBool::new(false),
                    fail_read: AtomicBool::new(false),
                    block_read: AtomicBool::new(false),
                    read_started: Notify::new(),
                    release_read: Notify::new(),
                    delay_consume: AtomicBool::new(false),
                    other: AtomicUsize::new(0),
                }
            })
        }

        async fn respond(&self, req: Request) -> Response {
            if req.uri().path() == CONSUME_PATH {
                assert_eq!(req.method(), axum::http::Method::POST);
                let auth = req
                    .headers()
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned);
                let account = req
                    .headers()
                    .get(crate::provider::CODEX_ACCOUNT_HEADER)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned);
                let agent_ok = req
                    .headers()
                    .get("user-agent")
                    .and_then(|v| v.to_str().ok())
                    == Some(concat!("tcr/", env!("CARGO_PKG_VERSION")));
                assert_eq!(
                    req.headers().get("content-type").unwrap(),
                    "application/json"
                );
                let body: serde_json::Value =
                    serde_json::from_slice(&to_bytes(req.into_body(), 8192).await.unwrap())
                        .unwrap();
                assert!(
                    body.as_object()
                        .is_some_and(|o| o.len() == 1 && o.contains_key("redeem_request_id")),
                    "consume body must omit credit identity"
                );
                self.seen.lock().unwrap().push(Seen {
                    body: body.clone(),
                    auth,
                    account,
                    agent_ok,
                });
                self.consumes.fetch_add(1, Ordering::SeqCst);
                if self.delay_consume.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_secs(11)).await;
                }
                if self.first_401.swap(false, Ordering::SeqCst) {
                    return (StatusCode::UNAUTHORIZED, "fake-secret-sentinel").into_response();
                }
                if !self.status.is_success() {
                    return (
                        self.status,
                        [("location", format!("{}/redirect-target", self.url))],
                        "fake-secret-sentinel",
                    )
                        .into_response();
                }
                if self.code == "slow_body" {
                    let stream = futures::stream::once(async {
                        tokio::time::sleep(Duration::from_secs(11)).await;
                        Ok::<_, std::io::Error>(r#"{"code":"reset"}"#)
                    });
                    return Response::new(axum::body::Body::from_stream(stream));
                }
                let key = body["redeem_request_id"].as_str().unwrap().to_owned();
                let code = if self.code == "reset" {
                    if self.redeemed.lock().unwrap().insert(key) {
                        self.debits.fetch_add(1, Ordering::SeqCst);
                        "reset"
                    } else {
                        "already_redeemed"
                    }
                } else {
                    self.code
                };
                if self.lose_first.swap(false, Ordering::SeqCst) {
                    return (StatusCode::OK, "fake-secret-sentinel").into_response();
                }
                return axum::Json(serde_json::json!({"code":code,"windows_reset":1,"ignored":"fake-secret-sentinel"})).into_response();
            }
            if req.uri().path() == crate::provider::CODEX_USAGE_PATH {
                self.reads.fetch_add(1, Ordering::SeqCst);
                if self.fail_read.load(Ordering::SeqCst) {
                    return (StatusCode::SERVICE_UNAVAILABLE, "fake-secret-sentinel")
                        .into_response();
                }
                let stale = self.block_read.swap(false, Ordering::SeqCst);
                if stale {
                    self.read_started.notify_one();
                    self.release_read.notified().await;
                }
                return axum::Json(serde_json::json!({"rate_limit":{"allowed":!stale,"limit_reached":stale,
                    "primary_window":{"used_percent":if stale {99} else {5},"limit_window_seconds":18000,
                    "reset_at":crate::now_ms()/1000 + 18000}},"rate_limit_reset_credits":{"available_count":0}})).into_response();
            }
            self.other.fetch_add(1, Ordering::SeqCst);
            (StatusCode::NOT_FOUND, "unexpected synthetic path").into_response()
        }

        fn manager(
            &self,
            accounts: Vec<Account>,
            refresher: Arc<dyn TokenRefresher>,
        ) -> Arc<Manager> {
            let mut config = Config::fixture(accounts);
            config.upstream = self.url.clone();
            config.codex_upstream = self.url.clone();
            let prober = LiveUsageProber::new().with_codex_base(self.url.clone());
            Manager::new(
                config,
                refresher,
                Arc::new(prober),
                Arc::new(LiveWarmer::new()),
                None,
            )
        }
    }

    fn row(name: &str) -> Account {
        Account {
            provider: Provider::Codex,
            account_uuid: Some(format!("fake-{name}")),
            ..Account::fixture(name)
        }
    }
    fn manager(backend: &Backend) -> Arc<Manager> {
        backend.manager(vec![row("alice@example.com")], Arc::new(oauth::NoRefresh))
    }
    fn selected(manager: &Manager) -> Option<usize> {
        manager.select(
            &HashSet::new(),
            OffsetDateTime::now_utc(),
            None,
            None,
            crate::provider::CODEX_RESPONSES_PATH,
            None,
        )
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reset_logs_do_not_expose_backend_identity_key_or_body() {
        let logs = crate::test_support::log_capture::SharedBuf::default();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .with_writer(logs.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        tracing::info!("reset privacy positive control");
        let backend = Backend::new("unknown", StatusCode::OK);
        let manager = manager(&backend);
        assert_eq!(
            manager
                .reset_codex("alice@example.com", "synthetic-attempt")
                .await,
            Err(CodexResetError::Unknown)
        );
        let text = logs.contents();
        assert!(text.contains("reset privacy positive control"));
        for private in [
            "fake-secret-sentinel",
            "synthetic-attempt",
            "fake-alice@example.com",
            "Bearer at-alice@example.com",
        ] {
            assert!(
                !text.contains(private),
                "reset logs must not disclose private request or response fields"
            );
        }
    }

    #[tokio::test]
    async fn reset_reads_usage_after_every_outcome_and_updates_selection() {
        for (code, expected) in [
            ("reset", CodexResetOutcome::Applied),
            ("nothing_to_reset", CodexResetOutcome::NothingToReset),
            ("no_credit", CodexResetOutcome::NoCredit),
            ("already_redeemed", CodexResetOutcome::AlreadyRedeemed),
        ] {
            let backend = Backend::new(code, StatusCode::OK);
            let manager = manager(&backend);
            let old = crate::codex_probe::usage_from_payload(
                &serde_json::json!({"rate_limit":{"allowed":false,"limit_reached":true,
                "primary_window":{"used_percent":100,"limit_window_seconds":18000,"reset_at":crate::now_ms()/1000+18000}}}),
            );
            manager.apply_usage(0, &old);
            assert_eq!(selected(&manager), None);
            let result = manager
                .reset_codex("alice@example.com", "synthetic-attempt")
                .await
                .unwrap();
            assert_eq!(result.outcome, expected);
            assert!(
                result.usage_refreshed,
                "recognized outcomes must refresh usage"
            );
            assert_eq!(
                backend.reads.load(Ordering::SeqCst),
                1,
                "every recognized outcome must read usage"
            );
            assert_eq!(
                selected(&manager),
                Some(0),
                "selection must see fresh windows"
            );
            assert!(backend.seen.lock().unwrap()[0].agent_ok);
        }
    }

    #[tokio::test]
    async fn reset_failed_post_read_keeps_known_outcome() {
        let backend = Backend::new("reset", StatusCode::OK);
        backend.fail_read.store(true, Ordering::SeqCst);
        let result = manager(&backend)
            .reset_codex("alice@example.com", "synthetic-attempt")
            .await
            .unwrap();
        assert_eq!(
            result,
            CodexResetResponse {
                outcome: CodexResetOutcome::Applied,
                usage_refreshed: false
            }
        );
        assert_eq!(backend.reads.load(Ordering::SeqCst), 1);
    }

    struct Freshener(AtomicUsize);
    impl TokenRefresher for Freshener {
        fn refresh(&self, provider: Provider, _refresh: String) -> oauth::RefreshFuture {
            assert_eq!(provider, Provider::Codex);
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Ok(Tokens {
                    id_token: None,
                    access_token: "fake-fresh-bearer".into(),
                    refresh_token: Some("fake-refresh".into()),
                    expires_at_ms: crate::now_ms() + 3_600_000,
                })
            })
        }
    }

    #[tokio::test]
    async fn reset_401_retry_keeps_key_body_and_named_login() {
        let backend = Backend::new("reset", StatusCode::OK);
        backend.first_401.store(true, Ordering::SeqCst);
        let freshener = Arc::new(Freshener(AtomicUsize::new(0)));
        let mut alice = row("alice@example.com");
        alice.org_uuid = Some("fake-workspace".into());
        let manager = backend.manager(
            vec![alice, row("bob@example.com")],
            Arc::<Freshener>::clone(&freshener),
        );
        assert_eq!(
            manager
                .reset_codex("alice@example.com", "synthetic-attempt")
                .await
                .unwrap()
                .outcome,
            CodexResetOutcome::Applied
        );
        assert_eq!(freshener.0.load(Ordering::SeqCst), 1);
        {
            let seen = backend.seen.lock().unwrap();
            assert_eq!(seen.len(), 2);
            assert!(
                seen[0].body == seen[1].body,
                "retry must retain the same attempt key and body"
            );
            assert!(
                seen.iter()
                    .all(|s| s.account.as_deref() == Some("fake-workspace")),
                "retry must retain named account identity"
            );
            assert!(
                seen[0].auth.as_deref() == Some("Bearer at-alice@example.com"),
                "the named bearer must match without being printed"
            );
            assert!(
                seen[1].auth.as_deref() == Some("Bearer fake-fresh-bearer"),
                "the named bearer must match without being printed"
            );
        }
        assert_eq!(
            manager
                .reset_codex("bob@example.com", "another-attempt")
                .await
                .unwrap()
                .outcome,
            CodexResetOutcome::Applied
        );
        let seen = backend.seen.lock().unwrap();
        assert!(
            seen.last().unwrap().auth.as_deref() == Some("Bearer at-bob@example.com"),
            "the second named bearer must match privately"
        );
        assert!(
            seen.last().unwrap().account.as_deref() == Some("fake-bob@example.com"),
            "the second named account must match privately"
        );
    }

    #[tokio::test]
    async fn reset_lost_response_and_concurrent_retry_debit_once() {
        let backend = Backend::new("reset", StatusCode::OK);
        backend.lose_first.store(true, Ordering::SeqCst);
        let manager = manager(&backend);
        assert_eq!(
            manager
                .reset_codex("alice@example.com", "synthetic-attempt")
                .await,
            Err(CodexResetError::Unknown)
        );
        let (a, b) = tokio::join!(
            manager.reset_codex("alice@example.com", "synthetic-attempt"),
            manager.reset_codex("alice@example.com", "synthetic-attempt")
        );
        assert_eq!(a.unwrap().outcome, CodexResetOutcome::AlreadyRedeemed);
        assert_eq!(b.unwrap().outcome, CodexResetOutcome::AlreadyRedeemed);
        assert_eq!(
            backend.debits.load(Ordering::SeqCst),
            1,
            "same-key retries must debit once"
        );
    }

    #[tokio::test]
    async fn reset_backend_failures_are_bounded_sanitized_and_never_redirected() {
        for (code, status, expected) in [
            ("unknown", StatusCode::OK, CodexResetError::Unknown),
            ("reset", StatusCode::UNAUTHORIZED, CodexResetError::Failed),
            (
                "reset",
                StatusCode::SERVICE_UNAVAILABLE,
                CodexResetError::Unknown,
            ),
            (
                "reset",
                StatusCode::TEMPORARY_REDIRECT,
                CodexResetError::Failed,
            ),
        ] {
            let backend = Backend::new(code, status);
            let result = manager(&backend)
                .reset_codex("alice@example.com", "synthetic-attempt")
                .await;
            assert_eq!(result, Err(expected));
            assert_eq!(
                backend.consumes.load(Ordering::SeqCst),
                1,
                "only a successful forced refresh permits one retry"
            );
            assert_eq!(
                backend.other.load(Ordering::SeqCst),
                0,
                "consume must never follow redirects"
            );
            assert!(!expected.message().contains("fake-secret-sentinel"));
        }
        let backend = Backend::new("reset", StatusCode::OK);
        let mut config = Config::fixture(vec![row("alice@example.com")]);
        config.codex_upstream = "http://127.0.0.1:0".into();
        let manager = Manager::new(
            config,
            Arc::new(oauth::NoRefresh),
            Arc::new(LiveUsageProber::new().with_codex_base(backend.url.clone())),
            Arc::new(LiveWarmer::new()),
            None,
        );
        assert_eq!(
            manager
                .reset_codex("alice@example.com", "synthetic-attempt")
                .await,
            Err(CodexResetError::Unknown)
        );
    }

    #[tokio::test]
    async fn reset_complete_call_timeout_is_unknown() {
        let backend = Backend::new("reset", StatusCode::OK);
        backend.delay_consume.store(true, Ordering::SeqCst);
        assert_eq!(
            manager(&backend)
                .reset_codex("alice@example.com", "synthetic-attempt")
                .await,
            Err(CodexResetError::Unknown)
        );
        assert_eq!(backend.consumes.load(Ordering::SeqCst), 1);
        assert_eq!(backend.reads.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn reset_body_timeout_is_also_unknown_and_401_retries_at_most_once() {
        let backend = Backend::new("slow_body", StatusCode::OK);
        assert_eq!(
            manager(&backend)
                .reset_codex("alice@example.com", "synthetic-attempt")
                .await,
            Err(CodexResetError::Unknown)
        );
        assert_eq!(backend.consumes.load(Ordering::SeqCst), 1);
        let backend = Backend::new("reset", StatusCode::UNAUTHORIZED);
        let freshener = Arc::new(Freshener(AtomicUsize::new(0)));
        let manager = backend.manager(
            vec![row("alice@example.com")],
            Arc::<Freshener>::clone(&freshener),
        );
        assert_eq!(
            manager
                .reset_codex("alice@example.com", "synthetic-attempt")
                .await,
            Err(CodexResetError::Failed)
        );
        assert_eq!(backend.consumes.load(Ordering::SeqCst), 2);
        assert_eq!(freshener.0.load(Ordering::SeqCst), 1);
    }

    struct PausedFreshener {
        started: Arc<Notify>,
        release: Arc<Notify>,
    }
    impl TokenRefresher for PausedFreshener {
        fn refresh(&self, _provider: Provider, _refresh: String) -> oauth::RefreshFuture {
            let started = Arc::clone(&self.started);
            let release = Arc::clone(&self.release);
            Box::pin(async move {
                started.notify_one();
                release.notified().await;
                Ok(Tokens {
                    id_token: None,
                    access_token: "fake-fresh-bearer".into(),
                    refresh_token: Some("fake-refresh".into()),
                    expires_at_ms: crate::now_ms() + 3_600_000,
                })
            })
        }
    }

    #[tokio::test]
    async fn reset_revalidates_after_refresh_and_before_post() {
        for release_owner in [false, true] {
            let backend = Backend::new("reset", StatusCode::OK);
            let started = Arc::new(Notify::new());
            let release = Arc::new(Notify::new());
            let mut expired = row("alice@example.com");
            expired.expires_at = Some(1);
            let manager = backend.manager(
                vec![expired],
                Arc::new(PausedFreshener {
                    started: Arc::clone(&started),
                    release: Arc::clone(&release),
                }),
            );
            let reset = {
                let manager = Arc::clone(&manager);
                tokio::spawn(async move {
                    manager
                        .reset_codex("alice@example.com", "synthetic-attempt")
                        .await
                })
            };
            started.notified().await;
            if release_owner {
                manager.release_mutation_ownership();
            } else {
                manager.accounts.write().unwrap()[0].org_uuid = Some("replacement".into());
            }
            release.notify_one();
            assert_eq!(reset.await.unwrap(), Err(CodexResetError::Unavailable));
            assert_eq!(backend.consumes.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    async fn reset_refuses_missing_ambiguous_wrong_provider_and_released_owner() {
        let backend = Backend::new("reset", StatusCode::OK);
        let manager = manager(&backend);
        assert_eq!(
            manager.reset_codex("alice", "synthetic-attempt").await,
            Err(CodexResetError::Missing)
        );
        manager.accounts.write().unwrap()[0].provider = Provider::Anthropic;
        assert_eq!(
            manager
                .reset_codex("alice@example.com", "synthetic-attempt")
                .await,
            Err(CodexResetError::WrongProvider)
        );
        manager.accounts.write().unwrap()[0].provider = Provider::Codex;
        manager.accounts.write().unwrap()[0].removed = true;
        assert_eq!(
            manager
                .reset_codex("alice@example.com", "synthetic-attempt")
                .await,
            Err(CodexResetError::Missing)
        );
        manager.accounts.write().unwrap()[0].removed = false;
        let duplicate = manager.accounts.read().unwrap()[0].clone();
        manager.accounts.write().unwrap().push(duplicate);
        assert_eq!(
            manager
                .reset_codex("alice@example.com", "synthetic-attempt")
                .await,
            Err(CodexResetError::Ambiguous)
        );
        manager.accounts.write().unwrap().pop();
        manager.release_mutation_ownership();
        assert_eq!(
            manager
                .reset_codex("alice@example.com", "synthetic-attempt")
                .await,
            Err(CodexResetError::Unavailable)
        );
        assert_eq!(backend.consumes.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn reset_revalidates_identity_after_gate_wait() {
        for mutation in 0..5 {
            let backend = Backend::new("reset", StatusCode::OK);
            let manager = manager(&backend);
            let gate = manager.usage_gate(0).unwrap();
            let guard = Arc::clone(&gate).lock_owned().await;
            let task = {
                let manager = Arc::clone(&manager);
                tokio::spawn(async move {
                    manager
                        .reset_codex("alice@example.com", "synthetic-attempt")
                        .await
                })
            };
            tokio::time::timeout(Duration::from_secs(2), async {
                while Arc::strong_count(&gate) < 4 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            {
                let mut rows = manager.accounts.write().unwrap();
                match mutation {
                    0 => rows[0].removed = true,
                    1 => rows[0].account_uuid = Some("replacement".into()),
                    2 => rows[0].org_uuid = Some("replacement".into()),
                    3 => rows[0].provider = Provider::Anthropic,
                    _ => rows[0].name = "replacement".into(),
                }
            }
            drop(guard);
            assert_eq!(task.await.unwrap(), Err(CodexResetError::Unavailable));
            assert_eq!(backend.consumes.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    async fn reset_gate_is_per_account_including_rows_added_after_start() {
        let backend = Backend::new("reset", StatusCode::OK);
        let manager = manager(&backend);
        manager
            .accounts
            .write()
            .unwrap()
            .push(AccountRuntime::from_config(&row("bob@example.com"), false));
        let guard = manager.usage_gate(0).unwrap().lock_owned().await;
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            manager.reset_codex("bob@example.com", "synthetic-attempt"),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(result.usage_refreshed);
        assert_eq!(
            backend.consumes.load(Ordering::SeqCst),
            1,
            "one held account must not block another"
        );
        drop(guard);
    }

    #[tokio::test]
    async fn reset_waits_for_old_probe_before_consuming_and_fresh_read_wins() {
        let backend = Backend::new("reset", StatusCode::OK);
        backend.block_read.store(true, Ordering::SeqCst);
        let manager = manager(&backend);
        let probe = {
            let manager = Arc::clone(&manager);
            tokio::spawn(async move { manager.probe_account(0).await })
        };
        backend.read_started.notified().await;
        let reset = {
            let manager = Arc::clone(&manager);
            tokio::spawn(async move {
                manager
                    .reset_codex("alice@example.com", "synthetic-attempt")
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(
            backend.consumes.load(Ordering::SeqCst),
            0,
            "reset must wait for older usage reads"
        );
        backend.release_read.notify_one();
        probe.await.unwrap();
        assert!(reset.await.unwrap().unwrap().usage_refreshed);
        assert_eq!(
            selected(&manager),
            Some(0),
            "older reads must not overwrite fresh reset windows"
        );
    }

    #[tokio::test]
    async fn reset_deadline_includes_gate_wait_and_preserves_known_outcome() {
        let backend = Backend::new("reset", StatusCode::OK);
        backend.block_read.store(true, Ordering::SeqCst);
        let manager = manager(&backend);
        let guard = manager.usage_gate(0).unwrap().lock_owned().await;
        let reset = {
            let manager = Arc::clone(&manager);
            tokio::spawn(async move {
                manager
                    .reset_codex("alice@example.com", "synthetic-attempt")
                    .await
            })
        };
        tokio::time::sleep(Duration::from_secs(85)).await;
        drop(guard);
        let result = tokio::time::timeout(Duration::from_secs(8), reset)
            .await
            .expect("the whole reset must include its gate wait in the deadline")
            .unwrap()
            .unwrap();
        backend.release_read.notify_one();
        assert_eq!(
            result,
            CodexResetResponse {
                outcome: CodexResetOutcome::Applied,
                usage_refreshed: false
            }
        );
        assert_eq!(backend.consumes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn reset_probes_selection_and_schedule_never_consume() {
        let backend = Backend::new("reset", StatusCode::OK);
        let manager = manager(&backend);
        manager.probe_account(0).await;
        manager.probe_all().await;
        assert_eq!(selected(&manager), Some(0));
        let snapshot = manager.snapshot(OffsetDateTime::now_utc());
        let urgency = crate::schedule::ProbeUrgency::of(&snapshot.accounts[0]);
        assert!(crate::schedule::probe_interval_secs(300, urgency, 10) > 0);
        let mut schedule = crate::schedule::FleetSchedule::capped(1);
        let mut rng = crate::schedule::Rng::from_clock();
        schedule.sync(crate::now_ms(), &[0], 300, &mut rng);
        for idx in schedule.due(crate::now_ms() + 600_000) {
            manager.probe_account(idx).await;
        }
        assert_eq!(
            backend.consumes.load(Ordering::SeqCst),
            0,
            "probes selection and schedule must never consume"
        );
        assert!(
            backend.reads.load(Ordering::SeqCst) >= 2,
            "positive control: automatic reads reached the backend"
        );
        manager
            .reset_codex("alice@example.com", "synthetic-attempt")
            .await
            .unwrap();
        assert_eq!(
            backend.consumes.load(Ordering::SeqCst),
            1,
            "positive control: confirmed reset must consume"
        );
    }
}
