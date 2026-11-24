//! Confirmed Claude banked-reset claims ([`crate::claude_resets`]). The usage probe reads the
//! bank; nothing here runs unless an operator confirmed one reset of one account.
//!
//! The shape is [`super::codex_reset`]'s: the account's usage gate, an owner recheck after
//! every await, one 90 s deadline, a usage read afterwards so the bars drop at once, one INFO
//! line per attempt naming the account by its tcr index only, and fixed reasons, never the
//! provider's body or a token.
use super::freshness::probing::BoundAccount;
use super::*;
use crate::claude_resets::PickError;
use crate::provider::Provider;
use std::time::Duration;
use tcr_status_wire::ResetKind;
use tokio::time::{Instant, timeout_at};

/// One recognized claim result. The words are the fixed sentences the panel and the CLI show.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ClaudeResetOutcome {
    /// The reset was spent and the windows it refills are empty.
    Applied,
    /// This attempt's reset was already spent.
    AlreadyUsed,
    /// Nothing was over its limit to refill (a grant that needs the limit first).
    NothingToReset,
    /// The grant is cooling down: usable later.
    NotUsableYet,
    /// Anthropic holds no usable reset for this account.
    NoResetLeft,
}

impl ClaudeResetOutcome {
    pub const fn message(self) -> &'static str {
        match self {
            Self::Applied => "Reset used.",
            Self::AlreadyUsed => "Already used.",
            Self::NothingToReset => "Nothing to reset.",
            Self::NotUsableYet => "Not usable yet.",
            Self::NoResetLeft => "No reset left.",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClaudeResetResponse {
    pub outcome: ClaudeResetOutcome,
    /// The kind of the grant claimed.
    pub kind: ResetKind,
    /// How many resets of that grant Anthropic says are left, when it said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_left: Option<u32>,
    pub usage_refreshed: bool,
}

/// Fixed reasons only: a transport error or a provider body could carry a credential.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaudeResetError {
    Missing,
    Ambiguous,
    WrongProvider,
    Unavailable,
    /// The last usage read did not say what the account holds: never read, or ineligible.
    NotRead,
    /// No reset of the asked kind (or none at all) is held.
    NoneHeld,
    /// No kind was asked and the account holds several.
    KindNeeded,
    /// The login was refused (401/403 after one refresh), or names no organization.
    Auth,
    Failed,
    Unknown,
}

impl ClaudeResetError {
    pub const fn message(self) -> &'static str {
        match self {
            Self::Missing => "The named account is not listed.",
            Self::Ambiguous => "The account name is ambiguous.",
            Self::WrongProvider => "The named account must be a Claude account.",
            Self::Unavailable => "The account or mutation owner changed.",
            Self::NotRead => "Banked resets unknown for this account; check again in a minute.",
            Self::NoneHeld => "No reset left.",
            Self::KindNeeded => "This account holds more than one kind of reset; choose one.",
            Self::Auth => "This login can't use resets; run tcr login again.",
            Self::Failed => "The reset request was refused.",
            Self::Unknown => "Couldn't confirm the reset went through; check again in a minute.",
        }
    }
}

/// A failed attempt: the fixed reason the caller sees, and for the tcr log only its cause
/// (a status code, a deadline, or a reqwest error with its URL removed).
#[derive(Debug)]
struct ClaimFailure {
    reason: ClaudeResetError,
    cause: Option<String>,
}

impl From<ClaudeResetError> for ClaimFailure {
    fn from(reason: ClaudeResetError) -> Self {
        Self {
            reason,
            cause: None,
        }
    }
}

impl ClaimFailure {
    const fn because(reason: ClaudeResetError, cause: String) -> Self {
        Self {
            reason,
            cause: Some(cause),
        }
    }

    fn transport(reason: ClaudeResetError, error: reqwest::Error) -> Self {
        Self::because(
            reason,
            crate::codex_probe::error_chain(&error.without_url()),
        )
    }
}

/// The claim's answer, read for its result and count only.
#[derive(serde::Deserialize)]
struct ClaimAnswer {
    result: String,
    #[serde(default)]
    resets_left: Option<u32>,
}

fn outcome_of(result: &str) -> Option<ClaudeResetOutcome> {
    match result {
        "reset" => Some(ClaudeResetOutcome::Applied),
        "already_used" => Some(ClaudeResetOutcome::AlreadyUsed),
        "not_limited" => Some(ClaudeResetOutcome::NothingToReset),
        "cooldown" => Some(ClaudeResetOutcome::NotUsableYet),
        "ineligible" | "unavailable" => Some(ClaudeResetOutcome::NoResetLeft),
        _ => None,
    }
}

impl Manager {
    /// The only path that spends a Claude banked reset. The caller must have confirmed
    /// operator input. `kind` picks the grant ([`crate::claude_resets::ClaudeResetBank::pick`]);
    /// `key` is the attempt's idempotency key, sent as the claim's `request_id`, so a retry
    /// of one attempt can never spend two resets.
    pub async fn reset_claude(
        &self,
        query: &str,
        kind: Option<ResetKind>,
        key: &str,
    ) -> Result<ClaudeResetResponse, ClaudeResetError> {
        self.reset_claude_at(ResetTarget::Name(query), kind, key)
            .await
    }

    /// [`Self::reset_claude`] for an account named by a user's query or by its rotation
    /// slot: the slot is how [`super::auto_reset`] names the account it chose itself.
    pub(super) async fn reset_claude_at(
        &self,
        target: ResetTarget<'_>,
        kind: Option<ResetKind>,
        key: &str,
    ) -> Result<ClaudeResetResponse, ClaudeResetError> {
        let mut index = None;
        let result = self.claim_claude_reset(target, kind, key, &mut index).await;
        let index = index.map_or_else(|| "none".to_owned(), |idx| idx.to_string());
        match &result {
            Ok(response) => tracing::info!(
                index = %index,
                kind = response.kind.token(),
                outcome = ?response.outcome,
                usage_refreshed = response.usage_refreshed,
                "claude reset attempt finished"
            ),
            Err(failure) => tracing::info!(
                index = %index,
                outcome = ?failure.reason,
                cause = failure.cause.as_deref().unwrap_or("none"),
                "claude reset attempt finished"
            ),
        }
        result.map_err(|failure| failure.reason)
    }

    async fn claim_claude_reset(
        &self,
        target: ResetTarget<'_>,
        kind: Option<ResetKind>,
        key: &str,
        index: &mut Option<usize>,
    ) -> Result<ClaudeResetResponse, ClaimFailure> {
        if !crate::claude_resets::valid_request_id(key) {
            return Err(ClaudeResetError::Failed.into());
        }
        let deadline = Instant::now() + Duration::from_secs(90);
        let claimed = timeout_at(deadline, async {
            let (idx, org) = {
                let accounts = self.accounts.read().unwrap_or_else(PoisonError::into_inner);
                let idx = match target.resolve(&accounts) {
                    crate::identity::Match::None => {
                        return Err(ClaimFailure::from(ClaudeResetError::Missing));
                    }
                    crate::identity::Match::Ambiguous(_) => {
                        return Err(ClaimFailure::from(ClaudeResetError::Ambiguous));
                    }
                    crate::identity::Match::One(idx) => idx,
                };
                *index = Some(idx);
                let row = &accounts[idx];
                if row.provider != Provider::Anthropic {
                    return Err(ClaimFailure::from(ClaudeResetError::WrongProvider));
                }
                (idx, row.org_uuid.clone())
            };
            let identity = self
                .bound_account(idx)
                .ok_or(ClaudeResetError::Unavailable)?;
            let org = org.filter(|org| !org.is_empty()).ok_or_else(|| {
                ClaimFailure::because(ClaudeResetError::Auth, "no organization on the row".into())
            })?;
            let gate = self.usage_gate(idx).ok_or(ClaudeResetError::Unavailable)?;
            let guard = gate.lock_owned().await;
            self.check_claude_owner(idx, &identity)?;
            self.ensure_fresh(idx).await;
            self.check_claude_owner(idx, &identity)?;
            let (grant_id, grant_kind) = {
                let accounts = self.accounts.read().unwrap_or_else(PoisonError::into_inner);
                let bank = accounts
                    .get(idx)
                    .and_then(|row| row.quota.claude_resets.as_ref())
                    .filter(|bank| bank.eligible)
                    .ok_or(ClaudeResetError::NotRead)?;
                let grant = bank.pick(kind).map_err(|error| match error {
                    PickError::NoneHeld => ClaudeResetError::NoneHeld,
                    PickError::KindNeeded => ClaudeResetError::KindNeeded,
                })?;
                (grant.id.clone(), grant.reset.kind)
            };
            let client = crate::http_client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(10))
                .build()
                .map_err(|error| ClaimFailure::transport(ClaudeResetError::Failed, error))?;
            let url = format!(
                "{}/api/organizations/{org}/reset_rate_limits",
                self.upstream_for(Provider::Anthropic).trim_end_matches('/')
            );
            let body = serde_json::json!({
                "program": crate::claude_resets::PROGRAM,
                "grant_id": grant_id,
                "request_id": key,
            });
            for attempt in 0..2 {
                self.check_claude_owner(idx, &identity)?;
                let token = self
                    .access_token(idx)
                    .ok_or(ClaudeResetError::Unavailable)?;
                let response = client
                    .post(&url)
                    .bearer_auth(token)
                    .header("anthropic-beta", crate::probe::OAUTH_USAGE_BETA)
                    .header(
                        reqwest::header::USER_AGENT,
                        crate::probe::CLAUDE_CODE_USER_AGENT,
                    )
                    .json(&body)
                    .send()
                    .await
                    .map_err(|error| ClaimFailure::transport(ClaudeResetError::Unknown, error))?;
                let status = response.status();
                let auth = status == reqwest::StatusCode::UNAUTHORIZED
                    || status == reqwest::StatusCode::FORBIDDEN;
                if auth && attempt == 0 {
                    self.check_claude_owner(idx, &identity)?;
                    if !self.ensure_fresh_force(idx).await {
                        return Err(ClaimFailure::because(
                            ClaudeResetError::Auth,
                            format!("{}, then the token refresh failed", status.as_u16()),
                        ));
                    }
                    continue;
                }
                let answered = format!("Anthropic answered {}", status.as_u16());
                if auth {
                    return Err(ClaimFailure::because(ClaudeResetError::Auth, answered));
                }
                if status.is_server_error() {
                    return Err(ClaimFailure::because(ClaudeResetError::Unknown, answered));
                }
                if !status.is_success() {
                    return Err(ClaimFailure::because(ClaudeResetError::Failed, answered));
                }
                let answer: ClaimAnswer = response
                    .json()
                    .await
                    .map_err(|error| ClaimFailure::transport(ClaudeResetError::Unknown, error))?;
                let outcome = outcome_of(&answer.result).ok_or_else(|| {
                    ClaimFailure::because(
                        ClaudeResetError::Unknown,
                        "a claim result this build cannot name".into(),
                    )
                })?;
                return Ok((
                    idx,
                    identity,
                    guard,
                    outcome,
                    grant_kind,
                    answer.resets_left,
                ));
            }
            Err(ClaimFailure::because(
                ClaudeResetError::Failed,
                "the claim loop ended without an answer".into(),
            ))
        })
        .await
        .map_err(|_| {
            ClaimFailure::because(
                ClaudeResetError::Unknown,
                "the 90 s reset deadline passed".into(),
            )
        })??;
        let (idx, identity, guard, outcome, kind, resets_left) = claimed;
        let usage_refreshed = if self.is_still_bound(idx, &identity) && !self.mutation_is_released()
        {
            timeout_at(deadline, self.probe_account_unlocked(idx))
                .await
                .unwrap_or(false)
                && self.is_still_bound(idx, &identity)
        } else {
            false
        };
        drop(guard);
        Ok(ClaudeResetResponse {
            outcome,
            kind,
            resets_left,
            usage_refreshed,
        })
    }

    fn check_claude_owner(
        &self,
        idx: usize,
        identity: &BoundAccount,
    ) -> Result<(), ClaudeResetError> {
        self.check_reset_owner(idx, identity)
            .map_err(|_| ClaudeResetError::Unavailable)
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

    const CLAIM_PATH: &str = "/api/organizations/fake-org/reset_rate_limits";

    /// A stand-in for api.anthropic.com: the usage read answers the grants it holds, and a
    /// claim spends one, by request id, at most once.
    struct Anthropic {
        url: String,
        claims: AtomicUsize,
        reads: AtomicUsize,
        weekly_left: Mutex<u32>,
        fable_left: u32,
        eligible: bool,
        result: &'static str,
        status: Mutex<StatusCode>,
        seen: Mutex<Vec<(serde_json::Value, bool, bool)>>,
        spent: Mutex<HashSet<String>>,
    }

    impl Anthropic {
        fn new(result: &'static str, eligible: bool, fable_left: u32) -> Arc<Self> {
            Arc::new_cyclic(|weak: &std::sync::Weak<Self>| {
                let weak = std::sync::Weak::clone(weak);
                let origin = stub::http(axum::Router::new().fallback(move |req: Request| {
                    let anthropic = weak.upgrade().unwrap();
                    async move { anthropic.respond(req).await }
                }));
                Self {
                    url: origin.url(),
                    claims: AtomicUsize::new(0),
                    reads: AtomicUsize::new(0),
                    weekly_left: Mutex::new(2),
                    fable_left,
                    eligible,
                    result,
                    status: Mutex::new(StatusCode::OK),
                    seen: Mutex::new(Vec::new()),
                    spent: Mutex::new(HashSet::new()),
                }
            })
        }

        fn grants(&self) -> serde_json::Value {
            let weekly = *self.weekly_left.lock().unwrap();
            serde_json::json!([
                {"id": "fake-weekly-late", "label": "fake", "type": "weekly", "resets_left": 1,
                 "usable_now": true, "use_requires_limit": false, "clears": ["seven_day"],
                 "ends_at": "2031-01-01T00:00:00Z"},
                {"id": "fake-weekly-early", "label": "fake", "type": "weekly",
                 "resets_left": weekly.saturating_sub(1), "usable_now": true,
                 "use_requires_limit": false, "clears": ["seven_day"],
                 "ends_at": "2030-01-01T00:00:00Z"},
                {"id": "fake-fable", "label": "fake", "type": "full", "gives_back_weekly": true,
                 "resets_left": self.fable_left, "usable_now": true, "use_requires_limit": false,
                 "clears": ["seven_day_overage_included"], "ends_at": "2030-06-01T00:00:00Z"},
            ])
        }

        async fn respond(&self, req: Request) -> Response {
            let agent_ok = req
                .headers()
                .get("user-agent")
                .and_then(|v| v.to_str().ok())
                == Some(crate::probe::CLAUDE_CODE_USER_AGENT);
            if req.uri().path() == "/api/oauth/usage" {
                self.reads.fetch_add(1, Ordering::SeqCst);
                assert_eq!(req.uri().query(), Some("cedar_ember=1"));
                let block = if self.eligible && agent_ok {
                    serde_json::json!({"eligible": true, "grants": self.grants(),
                        "next_grant_id": "fake-weekly-early"})
                } else {
                    serde_json::json!({"eligible": false, "ineligible_reason": "surface", "grants": []})
                };
                return axum::Json(serde_json::json!({
                    "five_hour": {"utilization": 10, "resets_at": "2030-01-01T00:00:00Z"},
                    "seven_day": {"utilization": 20, "resets_at": "2030-01-01T00:00:00Z"},
                    "cedar_ember": block,
                }))
                .into_response();
            }
            if req.uri().path() == CLAIM_PATH {
                assert_eq!(req.method(), axum::http::Method::POST);
                let beta_ok = req
                    .headers()
                    .get("anthropic-beta")
                    .and_then(|v| v.to_str().ok())
                    == Some(crate::probe::OAUTH_USAGE_BETA);
                let body: serde_json::Value =
                    serde_json::from_slice(&to_bytes(req.into_body(), 8192).await.unwrap())
                        .unwrap();
                self.seen
                    .lock()
                    .unwrap()
                    .push((body.clone(), agent_ok, beta_ok));
                self.claims.fetch_add(1, Ordering::SeqCst);
                let status = *self.status.lock().unwrap();
                if !status.is_success() {
                    return (status, "fake-secret-sentinel").into_response();
                }
                let request = body["request_id"].as_str().unwrap().to_owned();
                let result =
                    if self.result == "reset" && !self.spent.lock().unwrap().insert(request) {
                        "already_used"
                    } else {
                        if self.result == "reset" {
                            *self.weekly_left.lock().unwrap() -= 1;
                        }
                        self.result
                    };
                return axum::Json(serde_json::json!({"result": result, "resets_left": 0,
                    "ignored": "fake-secret-sentinel"}))
                .into_response();
            }
            (StatusCode::NOT_FOUND, "unexpected synthetic path").into_response()
        }

        fn manager(&self) -> Arc<Manager> {
            let mut config = Config::fixture(vec![row("alice@example.com")]);
            config.upstream = self.url.clone();
            let prober = LiveUsageProber::new().with_anthropic_base(&self.url);
            let manager = Manager::new(
                config,
                Arc::new(oauth::NoRefresh),
                Arc::new(prober),
                Arc::new(LiveWarmer::new()),
                None,
            );
            manager.set_plan_prober(Arc::new(NoPlan));
            manager
        }
    }

    struct NoPlan;
    impl PlanProber for NoPlan {
        fn plan(&self, _: String) -> crate::probe::PlanFuture {
            Box::pin(async { crate::probe::Plan::default() })
        }
    }

    fn row(name: &str) -> Account {
        Account {
            org_uuid: Some("fake-org".into()),
            ..Account::fixture(name)
        }
    }

    fn selected(manager: &Manager) -> Option<usize> {
        manager.select(
            &HashSet::new(),
            OffsetDateTime::now_utc(),
            None,
            None,
            "/v1/messages",
            None,
        )
    }

    /// The positive control the bridge asks for: probes, selection and the schedule read
    /// the bank and never claim; a confirmed reset claims exactly once.
    #[tokio::test]
    async fn probes_selection_and_schedule_never_claim() {
        let anthropic = Anthropic::new("reset", true, 1);
        let manager = anthropic.manager();
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
            anthropic.claims.load(Ordering::SeqCst),
            0,
            "automatic reads never claim"
        );
        assert!(
            anthropic.reads.load(Ordering::SeqCst) >= 2,
            "positive control: reads reached it"
        );
        let bank = snapshot.accounts[0].codex.banked_resets.clone().unwrap();
        assert!(bank.eligible);
        assert_eq!(bank.held().len(), 2, "weekly and Fable, read by the probe");
        manager
            .reset_claude(
                "alice@example.com",
                Some(ResetKind::Weekly),
                "synthetic-attempt",
            )
            .await
            .unwrap();
        assert_eq!(
            anthropic.claims.load(Ordering::SeqCst),
            1,
            "a confirmed reset claims"
        );
    }

    /// The claim names the asked kind's earliest grant, carries Claude Code's identity and the
    /// attempt key as its request id, and the usage read after it shows one fewer.
    #[tokio::test]
    async fn a_claim_spends_the_earliest_grant_of_the_kind_and_rereads_usage() {
        let anthropic = Anthropic::new("reset", true, 1);
        let manager = anthropic.manager();
        manager.probe_account(0).await;
        let response = manager
            .reset_claude(
                "alice@example.com",
                Some(ResetKind::Weekly),
                "synthetic-attempt",
            )
            .await
            .unwrap();
        assert_eq!(response.outcome, ClaudeResetOutcome::Applied);
        assert_eq!(response.kind, ResetKind::Weekly);
        assert!(response.usage_refreshed);
        {
            let seen = anthropic.seen.lock().unwrap();
            let (body, agent_ok, beta_ok) = &seen[0];
            assert_eq!(
                body,
                &serde_json::json!({"program": "cedar_ember", "grant_id": "fake-weekly-early",
                    "request_id": "synthetic-attempt"})
            );
            assert!(
                *agent_ok && *beta_ok,
                "the claim must present Claude Code's identity"
            );
        }
        let snapshot = manager.snapshot(OffsetDateTime::now_utc());
        let held = snapshot.accounts[0]
            .codex
            .banked_resets
            .clone()
            .unwrap()
            .held();
        assert_eq!(held[0], (ResetKind::Weekly, 1, Some(1_924_992_000_000)));
        // The same attempt again spends nothing more.
        let again = manager
            .reset_claude(
                "alice@example.com",
                Some(ResetKind::Weekly),
                "synthetic-attempt",
            )
            .await
            .unwrap();
        assert_eq!(again.outcome, ClaudeResetOutcome::AlreadyUsed);
    }

    #[tokio::test]
    async fn several_kinds_need_a_kind_and_one_kind_needs_none() {
        let anthropic = Anthropic::new("reset", true, 1);
        let manager = anthropic.manager();
        manager.probe_account(0).await;
        assert_eq!(
            manager
                .reset_claude("alice@example.com", None, "synthetic-attempt")
                .await,
            Err(ClaudeResetError::KindNeeded)
        );
        assert_eq!(
            manager
                .reset_claude(
                    "alice@example.com",
                    Some(ResetKind::Full),
                    "synthetic-attempt"
                )
                .await,
            Err(ClaudeResetError::NoneHeld)
        );
        assert_eq!(anthropic.claims.load(Ordering::SeqCst), 0);
        let single = Anthropic::new("reset", true, 0);
        let manager = single.manager();
        manager.probe_account(0).await;
        let response = manager
            .reset_claude("alice@example.com", None, "synthetic-attempt")
            .await
            .unwrap();
        assert_eq!(response.kind, ResetKind::Weekly);
    }

    /// A "surface" answer is unknown, not zero: no claim is sent, and the reason says so.
    #[tokio::test]
    async fn an_ineligible_bank_is_not_read_and_never_claims() {
        let anthropic = Anthropic::new("reset", false, 1);
        let manager = anthropic.manager();
        manager.probe_account(0).await;
        let snapshot = manager.snapshot(OffsetDateTime::now_utc());
        let bank = snapshot.accounts[0].codex.banked_resets.clone().unwrap();
        assert!(!bank.eligible);
        assert_eq!(bank.reason.as_deref(), Some("surface"));
        assert_eq!(
            manager
                .reset_claude(
                    "alice@example.com",
                    Some(ResetKind::Weekly),
                    "synthetic-attempt"
                )
                .await,
            Err(ClaudeResetError::NotRead)
        );
        assert_eq!(anthropic.claims.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn results_and_statuses_map_to_fixed_reasons() {
        for (result, outcome) in [
            ("not_limited", ClaudeResetOutcome::NothingToReset),
            ("cooldown", ClaudeResetOutcome::NotUsableYet),
            ("ineligible", ClaudeResetOutcome::NoResetLeft),
            ("unavailable", ClaudeResetOutcome::NoResetLeft),
        ] {
            let anthropic = Anthropic::new(result, true, 0);
            let manager = anthropic.manager();
            manager.probe_account(0).await;
            let response = manager
                .reset_claude("alice@example.com", None, "synthetic-attempt")
                .await
                .unwrap();
            assert_eq!(response.outcome, outcome, "{result}");
        }
        for (status, error) in [
            (StatusCode::TOO_MANY_REQUESTS, ClaudeResetError::Failed),
            (StatusCode::BAD_GATEWAY, ClaudeResetError::Unknown),
            (StatusCode::FORBIDDEN, ClaudeResetError::Auth),
        ] {
            let anthropic = Anthropic::new("reset", true, 0);
            let manager = anthropic.manager();
            manager.probe_account(0).await;
            *anthropic.status.lock().unwrap() = status;
            assert_eq!(
                manager
                    .reset_claude("alice@example.com", None, "synthetic-attempt")
                    .await,
                Err(error),
                "{status}"
            );
        }
    }

    /// One INFO line per attempt, naming the index and never the account, the key or the grant.
    #[tokio::test(flavor = "current_thread")]
    async fn each_attempt_logs_one_line_with_no_identity() {
        // Register both of the attempt's log callsites before capturing: a callsite first
        // reached by another thread with no subscriber is cached as never enabled, and a
        // rebuild only repairs callsites already registered (server.rs's boot-line test).
        let warm = Anthropic::new("reset", true, 1);
        let warm_manager = warm.manager();
        warm_manager.probe_account(0).await;
        assert!(
            warm_manager
                .reset_claude(
                    "alice@example.com",
                    Some(ResetKind::Weekly),
                    "warm-up-attempt"
                )
                .await
                .is_ok()
        );
        assert!(
            warm_manager
                .reset_claude("nobody@example.com", None, "warm-up-attempt")
                .await
                .is_err()
        );
        let logs = crate::test_support::log_capture::SharedBuf::default();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .with_writer(logs.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        tracing::callsite::rebuild_interest_cache();
        tracing::info!("claude reset privacy positive control");
        let anthropic = Anthropic::new("reset", true, 1);
        let manager = anthropic.manager();
        manager.probe_account(0).await;
        manager
            .reset_claude(
                "alice@example.com",
                Some(ResetKind::Weekly),
                "synthetic-attempt",
            )
            .await
            .unwrap();
        assert_eq!(
            manager
                .reset_claude("nobody@example.com", None, "synthetic-attempt")
                .await,
            Err(ClaudeResetError::Missing)
        );
        let text = logs.contents();
        assert!(text.contains("claude reset privacy positive control"));
        let lines: Vec<&str> = text
            .lines()
            .filter(|line| line.contains("claude reset attempt finished"))
            .collect();
        assert_eq!(lines.len(), 2, "{text}");
        assert!(
            lines[0].contains("index=0") && lines[0].contains("outcome=Applied"),
            "{}",
            lines[0]
        );
        assert!(
            lines[1].contains("index=none") && lines[1].contains("outcome=Missing"),
            "{}",
            lines[1]
        );
        // The probe's own lines name the account, as they always have; the attempt's
        // lines never do.
        for line in &lines {
            for private in ["alice@example.com", "nobody@example.com"] {
                assert!(!line.contains(private), "names an account: {line}");
            }
        }
        for private in [
            "synthetic-attempt",
            "fake-weekly-early",
            "fake-org",
            "fake-secret-sentinel",
            "Bearer",
        ] {
            assert!(!text.contains(private), "the logs disclose {private}");
        }
    }

    // MARK: automatic spend before the use-by (`super::super::auto_reset`)

    /// 2030-01-01T00:00:00Z, the fake's earliest weekly use-by.
    const WEEKLY_ENDS: i64 = 1_893_456_000_000;

    /// An hour before its use-by the expiring weekly reset is spent, once, under a key
    /// derived from the reset itself; a second tick spends nothing more, and the status
    /// wire says what happened.
    #[tokio::test]
    async fn an_expiring_reset_is_spent_once_before_its_use_by() {
        let anthropic = Anthropic::new("reset", true, 1);
        let manager = anthropic.manager();
        manager.probe_account(0).await;
        let now = WEEKLY_ENDS - 3_600_000;
        assert_eq!(manager.spend_expiring_resets(now).await, 1);
        assert_eq!(manager.spend_expiring_resets(now + 60_000).await, 0);
        assert_eq!(
            anthropic.claims.load(Ordering::SeqCst),
            1,
            "one reset, one claim"
        );
        {
            let seen = anthropic.seen.lock().unwrap();
            assert_eq!(
                seen[0].0,
                serde_json::json!({"program": "cedar_ember", "grant_id": "fake-weekly-early",
                    "request_id": "auto-0-weekly-1893456000000"})
            );
        }
        let snapshot = manager.snapshot(OffsetDateTime::now_utc());
        assert_eq!(
            snapshot.accounts[0].codex.auto_reset,
            Some(tcr_status_wire::AutoReset {
                kind: ResetKind::Weekly,
                at_ms: now,
                outcome: tcr_status_wire::AutoResetOutcome::Applied,
            })
        );
    }

    /// Three hours out nothing is due, and with `autoUseExpiringResets: false` nothing is
    /// spent even inside the window.
    #[tokio::test]
    async fn nothing_is_spent_early_or_when_switched_off() {
        let anthropic = Anthropic::new("reset", true, 1);
        let manager = anthropic.manager();
        manager.probe_account(0).await;
        assert_eq!(
            manager
                .spend_expiring_resets(WEEKLY_ENDS - 3 * 3_600_000)
                .await,
            0
        );
        manager.config.lock().unwrap().extra.insert(
            "autoUseExpiringResets".into(),
            serde_json::Value::Bool(false),
        );
        assert!(!manager.auto_reset_enabled());
        assert_eq!(
            manager.spend_expiring_resets(WEEKLY_ENDS - 3_600_000).await,
            0
        );
        assert_eq!(anthropic.claims.load(Ordering::SeqCst), 0);
        assert!(
            anthropic.reads.load(Ordering::SeqCst) >= 1,
            "positive control: the bank was read"
        );
    }

    /// A reset that needs the account at its limit first answers "nothing to reset": that
    /// is recorded as needing the limit and never retried.
    #[tokio::test]
    async fn a_reset_that_needs_the_limit_is_tried_once() {
        let anthropic = Anthropic::new("not_limited", true, 1);
        let manager = anthropic.manager();
        manager.probe_account(0).await;
        let now = WEEKLY_ENDS - 3_600_000;
        assert_eq!(manager.spend_expiring_resets(now).await, 1);
        assert_eq!(manager.spend_expiring_resets(now + 600_000).await, 0);
        assert_eq!(anthropic.claims.load(Ordering::SeqCst), 1);
        let snapshot = manager.snapshot(OffsetDateTime::now_utc());
        assert_eq!(
            snapshot.accounts[0]
                .codex
                .auto_reset
                .as_ref()
                .map(|a| a.outcome),
            Some(tcr_status_wire::AutoResetOutcome::NeedsLimit)
        );
    }
}
