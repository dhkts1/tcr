//! Usage probing: plan backfill, probe credentials, and how probe readings and failures land on a row.

use super::*;

/// A prober that succeeds on its first call (learning a 5-hour bar) and then
/// returns a 429 on every call after — for asserting a probe-429 keeps the
/// last-good utilization and reads as `RateLimited`, not `Error`.
struct FirstOkThen429Prober {
    calls: Arc<AtomicUsize>,
}

struct RetryAfterProber {
    calls: Arc<AtomicUsize>,
}

impl UsageProber for RetryAfterProber {
    fn probe(&self, _credential: crate::probe::ProbeCredential) -> ProbeFuture {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Err(ProbeError {
                status: Some(429),
                message: "Too Many Requests".into(),
                retry_after_secs: Some(3600),
            })
        })
    }
}
impl UsageProber for FirstOkThen429Prober {
    fn probe(&self, _credential: crate::probe::ProbeCredential) -> ProbeFuture {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if n == 0 {
                Ok(Usage {
                    five_hour: crate::probe::WindowReading::Read(UsageBucket {
                        utilization: Some(0.25),
                        reset_at_ms: Some(crate::now_ms() + 3_600_000),
                    }),
                    seven_day: None,
                    seven_day_oi: None,
                    extra_usage_usd: None,
                    codex: None,
                    claude_resets: None,
                })
            } else {
                Err(ProbeError {
                    status: Some(429),
                    message: "Too Many Requests".into(),
                    retry_after_secs: None,
                })
            }
        })
    }
}

/// One Anthropic OAuth row, `alice@example.com`, that has no plan yet.
const ALICE_WITHOUT_A_PLAN: &str = r#"{ "accounts": [
    { "name": "alice@example.com", "type": "oauth", "accessToken": "at-a",
      "refreshToken": "rt-a", "priority": 0 }
] }"#;

/// One Codex row, `bob@example.com/codex`, on the `plus` plan.
const BOB_CODEX_ON_PLUS: &str = r#"{ "accounts": [
    { "name": "bob@example.com/codex", "type": "oauth", "provider": "codex",
      "credentialVersion": 2,
      "accountUuid": "22222222-2222-4222-8222-222222222222",
      "organizationType": "plus", "groups": ["orchard"], "priority": 0,
      "accessToken": "at-bob", "refreshToken": "rt-bob",
      "expiresAt": 4102444800000 }
] }"#;

/// `json` written to a fresh temp config path tagged `tag`.
fn config_file(tag: &str, json: &str) -> PathBuf {
    let path = tmp_config_path(tag);
    std::fs::write(&path, json).unwrap();
    path
}

/// A manager over the config file at `path` that probes usage with `prober`.
fn manager_on_file(path: &std::path::Path, prober: Arc<dyn UsageProber>) -> Arc<Manager> {
    Manager::new(
        config::load(path).unwrap(),
        counting_refresher(),
        prober,
        Arc::new(NoWarmer),
        Some(path.to_path_buf()),
    )
}

/// The reason this feature has a backfill at all: fifteen accounts logged in
/// before the plan fields existed, and re-authenticating each in a browser is
/// not an answer. One probe learns the plan, writes it to the runtime row AND
/// to disk, and never asks again.
#[tokio::test]
async fn probing_backfills_a_missing_plan_once_and_persists_it() {
    let path = config_file("plan-backfill", ALICE_WITHOUT_A_PLAN);
    let manager = manager_on_file(&path, Arc::new(NoProber));

    let calls = count_plan_fetches(
        &manager,
        crate::probe::Plan {
            organization_type: Some("claude_team".to_string()),
            rate_limit_tier: Some("default_raven".to_string()),
            seat_tier: Some("team_standard".to_string()),
            plan_display_name: None,
        },
    );

    manager.probe_account(0).await;

    {
        let accounts = manager.accounts.read().expect("lock");
        assert_eq!(
            accounts[0].organization_type.as_deref(),
            Some("claude_team")
        );
        assert_eq!(accounts[0].seat_tier.as_deref(), Some("team_standard"));
    }
    let on_disk: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        on_disk["accounts"][0]["organizationType"], "claude_team",
        "a plan learned only in memory is re-fetched on every restart forever"
    );
    assert_eq!(on_disk["accounts"][0]["seatTier"], "team_standard");
    assert_eq!(calls.load(Ordering::Relaxed), 1);

    // The steady state: an account that HAS a plan is never asked again, so
    // this costs zero extra HTTP calls per cadence for the fleet's whole life.
    manager.probe_account(0).await;
    manager.probe_account(0).await;
    assert_eq!(
        calls.load(Ordering::Relaxed),
        1,
        "the backfill must run once per account, not once per probe"
    );
    std::fs::remove_file(&path).ok();
}

/// A row profiled before `plan_display_name` existed (2026-11-01) has a plan and no
/// display name. It is asked once more in this process, learns "Ultra" on the
/// runtime row and on disk, and is not asked again.
#[tokio::test]
async fn probing_asks_a_profiled_row_once_for_its_display_name() {
    let path = config_file(
        "plan-display-name",
        r#"{ "accounts": [
            { "name": "alice@example.com", "type": "oauth", "accessToken": "at-a",
              "refreshToken": "rt-a", "priority": 0,
              "organizationType": "claude_max", "rateLimitTier": "default_claude_max_20x" }
        ] }"#,
    );
    let manager = manager_on_file(&path, Arc::new(NoProber));
    let calls = count_plan_fetches(
        &manager,
        crate::probe::Plan {
            organization_type: Some("claude_max".to_string()),
            rate_limit_tier: Some("default_claude_max_20x".to_string()),
            seat_tier: None,
            plan_display_name: Some("Ultra".to_string()),
        },
    );

    manager.probe_account(0).await;
    manager.probe_account(0).await;

    assert_eq!(
        calls.load(Ordering::Relaxed),
        1,
        "asked once, not per probe"
    );
    assert_eq!(
        manager.accounts.read().expect("lock")[0]
            .plan_display_name
            .as_deref(),
        Some("Ultra")
    );
    let on_disk: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(on_disk["accounts"][0]["planDisplayName"], "Ultra");
    std::fs::remove_file(&path).ok();
}

/// A Codex usage prober whose every answer names one ChatGPT plan and nothing else.
struct CodexPlanProber {
    plan: &'static str,
}

impl UsageProber for CodexPlanProber {
    fn probe(&self, _credential: crate::probe::ProbeCredential) -> ProbeFuture {
        let plan = self.plan;
        Box::pin(async move {
            Ok(Usage {
                codex: Some(crate::codex_probe::CodexProbeFacts {
                    plan_type: Some(plan.into()),
                    ..Default::default()
                }),
                ..Usage::default()
            })
        })
    }
}

/// **A Codex account's plan follows its usage probe**, and the change is written to that
/// row's `organizationType` and to no other key. A refresh carries no new plan, so before
/// this a plan changed at ChatGPT (Plus to Pro here) stayed what the login said at import.
/// The write is the surgical `save_plan`, and an unchanged plan writes nothing at all.
///
/// Watched red, one break at a time: pass `Some("")` as the rate-limit tier to `save_plan`
/// and the document gains a `rateLimitTier`; drop the call in `probe_account` and the
/// runtime plan stays `plus`.
#[tokio::test]
async fn a_plus_to_pro_probe_updates_the_plan_and_saves_only_that_key() {
    let path = config_file("codex-plan", BOB_CODEX_ON_PLUS);
    let original = read_config_json(&path);
    let manager = manager_on_file(&path, Arc::new(CodexPlanProber { plan: "pro" }));
    // The Anthropic profile endpoint is never this plan's source: a Codex row skips the
    // backfill, and the counting fake keeps the live endpoint out should that ever break.
    let profile_calls = count_plan_fetches(&manager, crate::probe::Plan::default());

    manager.probe_account(0).await;

    assert_eq!(
        manager.accounts.read().expect("accounts lock poisoned")[0]
            .organization_type
            .as_deref(),
        Some("pro"),
        "the runtime row follows the probe's plan"
    );
    assert_eq!(
        manager.snapshot(OffsetDateTime::now_utc()).accounts[0]
            .plan_label()
            .as_deref(),
        Some("ChatGPT Pro")
    );
    let mut expected = original.clone();
    expected["accounts"][0]["organizationType"] = serde_json::json!("pro");
    assert_eq!(
        read_config_json(&path),
        expected,
        "the plan is saved, and no other key moves"
    );
    assert_eq!(profile_calls.load(Ordering::Relaxed), 0);

    let saved = std::fs::read(&path).unwrap();
    manager.probe_account(0).await;
    assert_eq!(
        std::fs::read(&path).unwrap(),
        saved,
        "an unchanged plan writes nothing"
    );
    std::fs::remove_file(&path).ok();
}

/// Run `probe_account(0)` on its own thread while this one holds `config_write`, and answer
/// the config file's bytes as they were while the lock was held, then as they are once it
/// was released and the probe finished. The bytes are read before the release, and asserted
/// on only after the join, so a writer that ignores the lock fails the test, not hangs it.
fn config_while_and_after_the_write_lock(
    manager: &Manager,
    path: &std::path::Path,
) -> (String, String) {
    let writing = manager
        .config_write
        .lock()
        .expect("config write lock poisoned");
    let held = std::thread::scope(|scope| {
        let probe = scope.spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("a runtime for the probe")
                .block_on(manager.probe_account(0));
        });
        // Long enough for the probe to reach its write, which must wait.
        std::thread::sleep(std::time::Duration::from_millis(500));
        let held = std::fs::read_to_string(path).unwrap();
        drop(writing);
        probe.join().expect("the probe finished");
        held
    });
    (held, std::fs::read_to_string(path).unwrap())
}

/// Nothing reaches the config file at `path` while another writer holds `config_write`,
/// and once it is released `probe_account(0)` has written `plan` to the row.
fn assert_the_plan_write_waits_for_the_write_lock(
    manager: &Manager,
    path: &std::path::Path,
    plan: &str,
) {
    let before = std::fs::read_to_string(path).unwrap();
    let (held, after) = config_while_and_after_the_write_lock(manager, path);
    assert_eq!(
        held, before,
        "nothing reaches the file while another writer holds the lock"
    );
    let after: serde_json::Value = serde_json::from_str(&after).unwrap();
    assert_eq!(
        after["accounts"][0]["organizationType"], plan,
        "once the lock is released, the plan is written"
    );
}

/// **The Codex plan write holds the config writers' lock** (INV1). `save_plan` reads the
/// whole document, changes one key and writes the document back, so without the lock a
/// read taken before another writer's rename and written after it puts back what that
/// writer replaced: a refresh token it had just rotated, which is single-use and already
/// spent, so that account is dead after its next restart. While another writer holds the
/// lock, nothing reaches the file; once it is released, the plan does.
///
/// Watched red: without the lock, the new plan is on disk while the lock is still held.
#[test]
fn a_codex_plan_write_waits_for_the_config_write_lock() {
    let path = tmp_config_path("codex-plan-lock");
    std::fs::write(
        &path,
        r#"{ "accounts": [
            { "name": "bob@example.com/codex", "type": "oauth", "provider": "codex",
              "credentialVersion": 2,
              "accountUuid": "22222222-2222-4222-8222-222222222222",
              "organizationType": "plus", "priority": 0,
              "accessToken": "at-bob", "refreshToken": "rt-bob",
              "expiresAt": 4102444800000 }
        ] }"#,
    )
    .unwrap();
    let manager = manager_on_file(&path, Arc::new(CodexPlanProber { plan: "pro" }));
    count_plan_fetches(&manager, crate::probe::Plan::default());

    assert_the_plan_write_waits_for_the_write_lock(&manager, &path, "pro");
    std::fs::remove_file(&path).ok();
}

/// The plan the profile backfill learns is written under the same lock, for the same
/// reason: it is the other writer `save_plan` has.
///
/// Watched red: without the lock, the learned plan is on disk while the lock is still held.
#[test]
fn a_backfilled_plan_write_waits_for_the_config_write_lock() {
    let path = config_file("plan-backfill-lock", ALICE_WITHOUT_A_PLAN);
    let manager = manager_on_file(&path, Arc::new(NoProber));
    count_plan_fetches(
        &manager,
        crate::probe::Plan {
            organization_type: Some("claude_team".to_string()),
            rate_limit_tier: None,
            seat_tier: None,
            plan_display_name: None,
        },
    );

    assert_the_plan_write_waits_for_the_write_lock(&manager, &path, "claude_team");
    std::fs::remove_file(&path).ok();
}

/// A Codex usage prober that answers with what the real parser makes of one payload.
struct CodexPayloadProber {
    payload: serde_json::Value,
}

impl UsageProber for CodexPayloadProber {
    fn probe(&self, _credential: crate::probe::ProbeCredential) -> ProbeFuture {
        let usage = crate::codex_probe::usage_from_payload(&self.payload);
        Box::pin(async move { Ok(usage) })
    }
}

/// **An empty `plan_type` names no plan.** A probe that answers `""`, or only whitespace,
/// leaves the account's plan as it was, in memory and on disk, so the plan label never goes
/// blank. A plan the probe does name is still taken verbatim.
///
/// Watched red: take `plan_type` with no emptiness check, and the row's plan becomes `""`.
#[tokio::test]
async fn a_probe_with_an_empty_plan_type_leaves_the_plan_as_it_was() {
    for plan_type in ["", "   "] {
        let path = config_file("codex-empty-plan", BOB_CODEX_ON_PLUS);
        let before = std::fs::read(&path).unwrap();
        let manager = manager_on_file(
            &path,
            Arc::new(CodexPayloadProber {
                payload: serde_json::json!({ "plan_type": plan_type, "rate_limit": null }),
            }),
        );
        count_plan_fetches(&manager, crate::probe::Plan::default());

        manager.probe_account(0).await;

        assert_eq!(
            manager.accounts.read().expect("accounts lock poisoned")[0]
                .organization_type
                .as_deref(),
            Some("plus"),
            "plan_type {plan_type:?} names no plan"
        );
        assert_eq!(
            manager.snapshot(OffsetDateTime::now_utc()).accounts[0]
                .plan_label()
                .as_deref(),
            Some("ChatGPT Plus"),
            "plan_type {plan_type:?}"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "plan_type {plan_type:?} writes nothing"
        );
        std::fs::remove_file(&path).ok();
    }
}

/// A profile fetch that failed says nothing about the account's quota or its
/// credential. Recording a probe failure over a cosmetic label would bench a
/// healthy account, so this path must be entirely silent on failure — and
/// must ask again next time rather than latching.
#[tokio::test]
async fn a_failed_plan_fetch_records_nothing_and_never_fails_the_probe() {
    let path = config_file("plan-fetch-fails", ALICE_WITHOUT_A_PLAN);
    let manager = manager_on_file(&path, Arc::new(NoProber));
    let before = std::fs::read_to_string(&path).unwrap();

    // An all-`None` reading is exactly what `fetch_profile` returns on
    // any failure — network, non-2xx, or a malformed body.
    let calls = count_plan_fetches(&manager, crate::probe::Plan::default());

    manager.probe_account(0).await;

    assert_eq!(
        manager.accounts.read().expect("lock")[0].organization_type,
        None,
        "nothing was learned, so nothing is recorded"
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        before,
        "a failed profile fetch must not rewrite the credential file"
    );

    manager.probe_account(0).await;
    assert_eq!(
        calls.load(Ordering::Relaxed),
        2,
        "a failure must not latch — the account still has no plan, so the \
         next probe asks again"
    );
    std::fs::remove_file(&path).ok();
}

/// An account whose plan came from config at boot is already answered, so
/// the probe loop never spends a request on it.
#[tokio::test]
async fn an_account_that_already_has_a_plan_is_never_re_fetched() {
    let path = config_file(
        "plan-already-known",
        r#"{ "accounts": [
            { "name": "alice@example.com", "type": "oauth", "accessToken": "at-a",
              "refreshToken": "rt-a", "priority": 0,
              "organizationType": "claude_max",
              "rateLimitTier": "default_claude_max_20x",
              "planDisplayName": "Ultra" }
        ] }"#,
    );
    let manager = manager_on_file(&path, Arc::new(NoProber));
    assert_eq!(
        manager.accounts.read().expect("lock")[0]
            .rate_limit_tier
            .as_deref(),
        Some("default_claude_max_20x"),
        "the plan is read from config at boot, not only learned by probing"
    );

    let calls = count_plan_fetches(&manager, crate::probe::Plan::default());

    manager.probe_account(0).await;
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    std::fs::remove_file(&path).ok();
}

/// An Anthropic row, `alice@example.com`, whose account uuid is all ones.
fn alice_with_an_account_uuid() -> Account {
    Account {
        account_uuid: Some("11111111-1111-4111-8111-111111111111".to_string()),
        ..account("alice@example.com", 0)
    }
}

/// Probe each of `accounts` once, in order, and answer the credentials the probes carried.
/// The plan backfill gets a counting fake, so no row asks the live profile endpoint.
async fn probe_credentials_of(accounts: Vec<Account>) -> Vec<crate::probe::ProbeCredential> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let count = accounts.len();
    let manager = build_manager_with_prober(
        config_with(accounts),
        counting_refresher(),
        Arc::new(RecordingProber {
            seen: Arc::clone(&seen),
        }),
    );
    count_plan_fetches(&manager, crate::probe::Plan::default());
    for idx in 0..count {
        manager.probe_account(idx).await;
    }
    seen.lock().expect("seen lock poisoned").clone()
}

/// **A probe names the account it is for, not just the bearer it pays with.** The
/// provider picks the usage endpoint, and a Codex probe must also send its ChatGPT
/// account id in `chatgpt-account-id`, which is the row's `account_uuid`. An Anthropic
/// row and a Codex row are probed, so each provider's credential is pinned beside the
/// other's.
///
/// Watched red: build the credential with `account_id: None` and the Codex row's
/// account id is missing.
#[tokio::test]
async fn a_probe_credential_names_the_accounts_provider_and_account_uuid() {
    let codex = Account {
        provider: crate::provider::Provider::Codex,
        account_uuid: Some("22222222-2222-4222-8222-222222222222".to_string()),
        ..account("bob@example.com", 1)
    };
    let seen: Vec<_> = probe_credentials_of(vec![alice_with_an_account_uuid(), codex])
        .await
        .iter()
        .map(|c| (c.provider, c.access_token.clone(), c.account_id.clone()))
        .collect();
    assert_eq!(
        seen,
        vec![
            (
                crate::provider::Provider::Anthropic,
                "at-alice@example.com".to_string(),
                Some("11111111-1111-4111-8111-111111111111".to_string()),
            ),
            (
                crate::provider::Provider::Codex,
                "at-bob@example.com".to_string(),
                Some("22222222-2222-4222-8222-222222222222".to_string()),
            ),
        ],
        "each probe carries its own row's provider, bearer and account id"
    );
}

/// **A Codex probe names the workspace.** A Codex row's `chatgpt-account-id` is its
/// workspace, `org_uuid`, since `account_uuid` became the ChatGPT user; a row saved before
/// that split carries the workspace in `account_uuid` and no `org_uuid`, and keeps sending
/// it. An Anthropic row's credential still names its `account_uuid`.
///
/// Watched red: with the Codex arm reading `account_uuid`, bob's probe names his user.
#[tokio::test]
async fn a_codex_probe_names_the_workspace_and_falls_back_to_the_account_uuid() {
    let codex = Account {
        provider: crate::provider::Provider::Codex,
        account_uuid: Some("55555555-5555-4555-8555-555555555555".to_string()),
        org_uuid: Some("66666666-6666-4666-8666-666666666666".to_string()),
        ..account("bob@example.com", 1)
    };
    let older = Account {
        provider: crate::provider::Provider::Codex,
        account_uuid: Some("77777777-7777-4777-8777-777777777777".to_string()),
        ..account("carol@example.com", 2)
    };
    let seen: Vec<_> = probe_credentials_of(vec![alice_with_an_account_uuid(), codex, older])
        .await
        .iter()
        .map(|c| (c.provider, c.account_id.clone()))
        .collect();
    assert_eq!(
        seen,
        vec![
            (
                crate::provider::Provider::Anthropic,
                Some("11111111-1111-4111-8111-111111111111".to_string()),
            ),
            (
                crate::provider::Provider::Codex,
                Some("66666666-6666-4666-8666-666666666666".to_string()),
            ),
            (
                crate::provider::Provider::Codex,
                Some("77777777-7777-4777-8777-777777777777".to_string()),
            ),
        ]
    );
}

/// **A row that is gone yields no credential, never an Anthropic one.** The provider
/// picks the usage endpoint, so a credential built on the default provider would send the
/// bearer to Anthropic's, whichever provider issued it. No row can vanish today, so this
/// pins the fallback rather than a path: an index past the end builds nothing, and
/// `probe_account` stands down on that (read, not driven: its token read already returns
/// early for such an index). The row that is there is the control.
///
/// Watched red: fall back to `Provider::default()` and the index past the end gets an
/// Anthropic credential.
#[test]
fn a_row_that_is_gone_yields_no_probe_credential() {
    let codex = Account {
        provider: crate::provider::Provider::Codex,
        account_uuid: Some("22222222-2222-4222-8222-222222222222".to_string()),
        ..account("bob@example.com", 0)
    };
    let manager = manager_of(vec![codex]);
    let gone = manager.probe_credential(1, "at-bob@example.com".to_string());
    assert!(
        gone.is_none(),
        "an index past the end has no provider to pick an endpoint by: {gone:?}"
    );
    let control = manager
        .probe_credential(0, "at-bob@example.com".to_string())
        .expect("control: the row that is there yields its credential");
    assert_eq!(control.provider, crate::provider::Provider::Codex);
    assert_eq!(
        control.account_id.as_deref(),
        Some("22222222-2222-4222-8222-222222222222")
    );
}

/// Finding #1: the background probe must SKIP disabled and errored accounts —
/// never spend upstream traffic on a sidelined account nor let a probe's
/// refresh silently flip an errored account back into rotation.
///
/// UPDATED contract (control account, part 1): "disabled → skipped" now
/// has exactly one exception, the control account, covered separately by
/// [`probeable_indices_still_includes_a_disabled_control_account`] and
/// [`probeable_indices_excludes_a_disabled_non_control_account_even_with_a_control_set`]
/// below. This test has no control account configured at all, so it keeps
/// asserting the base case: an ordinary disabled account is skipped.
#[tokio::test]
async fn probe_all_skips_disabled_and_errored_accounts() {
    let prober = Arc::new(ScriptedProber {
        ok_token: "at-ok".to_string(),
    });
    let manager = build_manager_with_prober(
        config_with(vec![
            account("ok", 0),
            account("off", 0),
            account("dead", 0),
        ]),
        counting_refresher(),
        prober,
    );
    manager.set_disabled(1, true);
    manager.mark_error(2);

    manager.probe_all().await;
    let snap = manager.snapshot(OffsetDateTime::now_utc());

    // Only the healthy account was probed.
    assert_eq!(snap.accounts[0].probe_status, ProbeStatus::Ok);
    assert_eq!(
        snap.accounts[1].probe_status,
        ProbeStatus::Never,
        "a disabled non-control account must not be probed"
    );
    assert!(snap.accounts[1].last_probe.is_none());
    assert_eq!(
        snap.accounts[2].probe_status,
        ProbeStatus::Never,
        "an errored account must not be probed (nor reactivated)"
    );
    // And the errored account was NOT flipped back to active by a probe refresh.
    assert_eq!(manager.account_status(2), Some(AccountStatus::Error));
}

/// The narrow exception itself, asserted directly on `probeable_indices()`
/// — the unit `probe_all` merely iterates: a disabled account that IS the
/// control account stays probeable, so its usage keeps getting tracked
/// even though it is deliberately out of the inference rotation.
#[test]
fn probeable_indices_still_includes_a_disabled_control_account() {
    let manager = manager_of(vec![account("ok", 0), account("gil", 0)]);
    manager.set_control_by_query(Some("gil"));
    manager.set_disabled(1, true);

    assert_eq!(
        manager.probeable_indices(),
        vec![0, 1],
        "the disabled CONTROL account (idx 1) must still be probeable"
    );
}

/// The exception must not silently widen: with a control account set,
/// every OTHER disabled account is still excluded from
/// `probeable_indices()`.
#[test]
fn probeable_indices_excludes_a_disabled_non_control_account_even_with_a_control_set() {
    let manager = manager_of(vec![account("gil", 0), account("ok", 0), account("off", 0)]);
    manager.set_control_by_query(Some("gil"));
    manager.set_disabled(0, true); // the control account itself, disabled
    manager.set_disabled(2, true); // an unrelated account, disabled

    assert_eq!(
        manager.probeable_indices(),
        vec![0, 1],
        "idx 0 (disabled control) stays probeable, idx 1 (enabled) stays \
         probeable, idx 2 (disabled, NOT control) is excluded — the \
         exception must not widen"
    );
}

/// The background probe refreshes EVERY OAuth row (not just the serving one)
/// and a FAILING probe surfaces as visible probe health — a message and a
/// timestamp — rather than a silently-frozen quota bar. A transient upstream
/// 5xx is a *soft* `RateLimited`, not a red error (the endpoint, not the
/// credential, is unhappy), and it must not fabricate a fresh bar.
#[tokio::test]
async fn probe_all_updates_every_account_and_records_failures() {
    // Account 0's token ("at-ok") probes OK; account 1's does not.
    let prober = Arc::new(ScriptedProber {
        ok_token: "at-ok".to_string(),
    });
    let manager = build_manager_with_prober(
        config_with(vec![account("ok", 0), account("bad", 0)]),
        counting_refresher(),
        prober,
    );

    manager.probe_all().await;
    let now = OffsetDateTime::now_utc();
    let snap = manager.snapshot(now);

    // Row 0: probe succeeded and its quota window was learned live.
    assert_eq!(snap.accounts[0].probe_status, ProbeStatus::Ok);
    assert!(snap.accounts[0].probe_error.is_none());
    assert!(snap.accounts[0].last_probe.is_some());
    assert_eq!(snap.accounts[0].five_hour, Some(0.25));

    // Row 1: the 5xx probe failed and the failure is visible, not hidden —
    // but as a soft RateLimited (transient upstream), never a red error.
    assert_eq!(snap.accounts[1].probe_status, ProbeStatus::RateLimited);
    assert!(snap.accounts[1].probe_error.is_some());
    assert!(snap.accounts[1].last_probe.is_some());
    // A failed probe must NOT fabricate a fresh bar for the bad account.
    assert_eq!(snap.accounts[1].five_hour, None);
}

/// `quotaUpdatedAtMs` is the time of the newest quota reading from EITHER
/// source, so a consumer can tell a reading too old to trust from a fresh
/// one. `lastProbeMs` alone cannot: a busy account's bars move from response
/// headers between probes, and an idle account with a wedged probe shows
/// frozen bars that `lastProbeMs` (stamped on failures too) reads as recent.
///
/// Walks the hops a consumer sees: the runtime `Quota`, the manager's
/// snapshot, and the `tcr.status.v1` payload the server sends the CLI.
#[tokio::test]
async fn quota_updated_at_is_the_newest_reading_from_probe_or_headers() {
    let manager = build_manager_with_prober(
        config_with(vec![account("ok", 0), account("bad", 0)]),
        counting_refresher(),
        Arc::new(ScriptedProber {
            ok_token: "at-ok".to_string(),
        }),
    );
    let wire_value = quota_updated_at_on_the_wire;

    // Nothing read yet: absent, never a fabricated "just now".
    assert_eq!(wire_value(&manager, 0), None);

    // A fresh account: a successful probe stamps a recent reading.
    let before = crate::now_ms();
    manager.probe_all().await;
    let fresh = wire_value(&manager, 0).expect("a probed account carries quotaUpdatedAtMs");
    assert!(
        fresh >= before && fresh <= crate::now_ms(),
        "a fresh probe reads as recent: {fresh} not in [{before}, now]"
    );
    // A FAILED probe is not a reading: `lastProbeMs` moves, this does not.
    assert!(
        manager.snapshot(OffsetDateTime::now_utc()).accounts[1]
            .last_probe
            .is_some()
    );
    assert_eq!(
        wire_value(&manager, 1),
        None,
        "a failed probe read no quota and must not claim one"
    );

    // A stale account: the last reading was 40 minutes ago, past any sane
    // freshness limit, and the wire says exactly when.
    let stale = crate::now_ms() - 40 * 60 * 1000;
    {
        let mut accounts = accounts_mut(&manager);
        accounts[0].quota.updated_at = Some(
            OffsetDateTime::from_unix_timestamp_nanos(i128::from(stale) * 1_000_000)
                .expect("valid instant"),
        );
    }
    assert_eq!(
        wire_value(&manager, 0),
        Some(stale),
        "the status row carries the exact time of the stale reading"
    );

    // A response with no rate-limit headers (an error page) is silence, not
    // a reading: the stale time stands.
    manager.update_quota(0, &reqwest::header::HeaderMap::new(), 502);
    assert_eq!(wire_value(&manager, 0), Some(stale));

    // A header update after the stale probe moves it forward.
    let before = crate::now_ms();
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        "anthropic-ratelimit-unified-5h-utilization",
        "0.30".parse().expect("header value"),
    );
    manager.update_quota(0, &headers, 200);
    let moved = wire_value(&manager, 0).expect("still present");
    assert!(
        moved >= before,
        "a 5h header reading moves quotaUpdatedAtMs forward: {moved} < {before}"
    );

    // A header update that returns `false` (no 5h window) but reads the 7d
    // window is still a reading, and also moves it forward.
    {
        let mut accounts = accounts_mut(&manager);
        accounts[0].quota.updated_at = Some(
            OffsetDateTime::from_unix_timestamp_nanos(i128::from(stale) * 1_000_000)
                .expect("valid instant"),
        );
    }
    let before = crate::now_ms();
    let mut weekly_only = reqwest::header::HeaderMap::new();
    weekly_only.insert(
        "anthropic-ratelimit-unified-7d-utilization",
        "0.40".parse().expect("header value"),
    );
    assert!(
        !manager.update_quota(0, &weekly_only, 200),
        "control: this response carries no 5h window"
    );
    let moved = wire_value(&manager, 0).expect("still present");
    assert!(
        moved >= before,
        "a 7d-only header reading moves quotaUpdatedAtMs forward: {moved} < {before}"
    );
}

/// Probe 429-tolerance: the usage endpoint throttling the *probe* is benign.
/// After a good probe learns a bar, a following 429 probe must (a) read as
/// `RateLimited`, never `Error`, and (b) leave the last-good utilization
/// intact — so a throttled sweep never blanks the bars nor cries false error.
#[tokio::test]
async fn probe_429_is_rate_limited_and_keeps_last_utilization() {
    let prober = Arc::new(FirstOkThen429Prober {
        calls: Arc::new(AtomicUsize::new(0)),
    });
    let manager = build_manager_with_prober(
        config_with(vec![account("a", 0)]),
        counting_refresher(),
        prober,
    );

    // First sweep: probe succeeds, the 5-hour bar is learned.
    manager.probe_all().await;
    let snap = manager.snapshot(OffsetDateTime::now_utc());
    assert_eq!(snap.accounts[0].probe_status, ProbeStatus::Ok);
    assert_eq!(snap.accounts[0].five_hour, Some(0.25));

    // Second sweep: the probe is 429'd. Soft state, and the bar is untouched.
    manager.probe_all().await;
    let snap = manager.snapshot(OffsetDateTime::now_utc());
    assert_eq!(
        snap.accounts[0].probe_status,
        ProbeStatus::RateLimited,
        "a probe 429 is soft, never a red error"
    );
    assert_eq!(
        snap.accounts[0].five_hour,
        Some(0.25),
        "a probe 429 must preserve the last-good utilization, not blank it"
    );
    assert!(snap.accounts[0].probe_error.is_some());
}

/// A prober that always fails with a fixed HTTP status — for pinning exactly
/// how many consecutive reads of a given status it takes (or does not take)
/// to move `probe_status` past the benign `RateLimited`.
struct AlwaysStatusProber {
    status: u16,
}
impl UsageProber for AlwaysStatusProber {
    fn probe(&self, _credential: crate::probe::ProbeCredential) -> ProbeFuture {
        let status = self.status;
        Box::pin(async move {
            Err(ProbeError {
                status: Some(status),
                message: "upstream boom".into(),
                retry_after_secs: None,
            })
        })
    }
}

/// 1b, the half of the fix `probe_429_is_rate_limited_and_keeps_last_utilization`
/// does not cover: ONE 5xx read is still the same benign, self-clearing
/// `RateLimited` a 429 gets — it must NOT jump straight to the visible
/// escalated state on the first hiccup.
#[tokio::test]
async fn a_single_5xx_probe_stays_rate_limited() {
    let manager = build_manager_with_prober(
        config_with(vec![account("a", 0)]),
        counting_refresher(),
        Arc::new(AlwaysStatusProber { status: 503 }),
    );

    manager.probe_all().await;
    let snap = manager.snapshot(OffsetDateTime::now_utc());
    assert_eq!(
        snap.accounts[0].probe_status,
        ProbeStatus::RateLimited,
        "a lone 5xx is a hiccup, same as a 429 — not yet the sustained-outage state"
    );
}

/// 1b, the other half: a SUSTAINED run of 5xx reads — as opposed to the
/// single hiccup above — is first-hand evidence the usage endpoint itself is
/// down, and must escalate to the visible `UpstreamDown` rather than keep
/// hiding behind `RateLimited` forever.
#[tokio::test]
async fn sustained_5xx_probes_escalate_to_upstream_down() {
    let manager = build_manager_with_prober(
        config_with(vec![account("a", 0)]),
        counting_refresher(),
        Arc::new(AlwaysStatusProber { status: 502 }),
    );

    for sweep in 1..probing::SUSTAINED_5XX_THRESHOLD {
        manager.probe_all().await;
        let snap = manager.snapshot(OffsetDateTime::now_utc());
        assert_eq!(
            snap.accounts[0].probe_status,
            ProbeStatus::RateLimited,
            "sweep {sweep}: below the threshold, still the benign label"
        );
    }

    // The crossing sweep.
    manager.probe_all().await;
    let snap = manager.snapshot(OffsetDateTime::now_utc());
    assert_eq!(
        snap.accounts[0].probe_status,
        ProbeStatus::UpstreamDown,
        "a sustained run of 5xx must surface as its own visible status"
    );
}

/// The counters driving the two statuses above must be genuinely
/// independent: a 429 never contributes to the sustained-5xx count, so an
/// account that is merely self-throttled — however many times in a row —
/// must never be mistaken for a real outage.
#[tokio::test]
async fn repeated_429s_never_escalate_to_upstream_down() {
    let manager = build_manager_with_prober(
        config_with(vec![account("a", 0)]),
        counting_refresher(),
        Arc::new(AlwaysStatusProber { status: 429 }),
    );

    for _ in 0..(probing::SUSTAINED_5XX_THRESHOLD * 3) {
        manager.probe_all().await;
    }
    let snap = manager.snapshot(OffsetDateTime::now_utc());
    assert_eq!(
        snap.accounts[0].probe_status,
        ProbeStatus::RateLimited,
        "a self-throttle stays benign no matter how many times it repeats"
    );
}

/// A success between two 5xx runs must reset the sustained-5xx count, the
/// same way it resets `consecutive_probe_failures` — a brief recovery in the
/// middle is not "the endpoint has been down the whole time".
#[tokio::test]
async fn a_success_between_5xx_runs_resets_the_sustained_count() {
    let state = Arc::new(Mutex::new(0usize));

    struct FlakyProber {
        state: Arc<Mutex<usize>>,
    }
    impl UsageProber for FlakyProber {
        fn probe(&self, _credential: crate::probe::ProbeCredential) -> ProbeFuture {
            let mut n = self.state.lock().expect("lock poisoned");
            *n += 1;
            // 5xx, 5xx, OK, 5xx, 5xx — never THREE in a row.
            let fail = matches!(*n, 1 | 2 | 4 | 5);
            let call = *n;
            drop(n);
            Box::pin(async move {
                if fail {
                    Err(ProbeError {
                        status: Some(500),
                        message: format!("upstream boom #{call}"),
                        retry_after_secs: None,
                    })
                } else {
                    Ok(Usage {
                        five_hour: crate::probe::WindowReading::Read(UsageBucket {
                            utilization: Some(0.1),
                            reset_at_ms: Some(crate::now_ms() + 3_600_000),
                        }),
                        seven_day: None,
                        seven_day_oi: None,
                        extra_usage_usd: None,
                        codex: None,
                        claude_resets: None,
                    })
                }
            })
        }
    }

    let manager = build_manager_with_prober(
        config_with(vec![account("a", 0)]),
        counting_refresher(),
        Arc::new(FlakyProber { state }),
    );

    for _ in 0..5 {
        manager.probe_all().await;
    }
    let snap = manager.snapshot(OffsetDateTime::now_utc());
    assert_eq!(
        snap.accounts[0].probe_status,
        ProbeStatus::RateLimited,
        "two 5xx, a success, then two more 5xx is never THREE IN A ROW — \
         the intervening success must have reset the count, or this reads \
         as UpstreamDown for a run that was never actually sustained"
    );
}

#[tokio::test]
async fn probe_retry_after_blocks_the_next_probe_until_it_expires() {
    let calls = Arc::new(AtomicUsize::new(0));
    let manager = build_manager_with_prober(
        config_with(vec![account("a", 0)]),
        counting_refresher(),
        Arc::new(RetryAfterProber {
            calls: Arc::clone(&calls),
        }),
    );

    manager.probe_all().await;
    manager.probe_all().await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    manager.accounts.write().unwrap()[0].probe_retry_after_ms = Some(crate::now_ms() - 1);
    manager.probe_all().await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[test]
fn probe_retry_after_has_a_seven_day_safety_cap() {
    let manager = manager_of(vec![account("a", 0)]);
    let before = crate::now_ms();

    manager.record_probe(
        0,
        ProbeStatus::RateLimited,
        Some("rate limited".to_string()),
        Some(u64::MAX),
    );

    let deadline = manager.accounts.read().unwrap()[0]
        .probe_retry_after_ms
        .unwrap();
    assert!(deadline >= before + 7 * 24 * 60 * 60 * 1000);
    assert!(deadline <= crate::now_ms() + 7 * 24 * 60 * 60 * 1000);
}

/// A network/transport probe failure (no HTTP status, not a timeout) stays a
/// VISIBLE `Error` — a persistent connectivity problem (upstream down, DNS/TLS,
/// a proxy-env regression) must never hide behind a benign "busy". Only
/// endpoint-side 429/5xx soften; a transport failure is the credential's whole
/// path being down, exactly the signal probe-health exists to surface.
#[tokio::test]
async fn probe_transport_failure_is_visible_error() {
    struct NetFailProber;
    impl UsageProber for NetFailProber {
        fn probe(&self, _credential: crate::probe::ProbeCredential) -> ProbeFuture {
            Box::pin(async {
                Err(ProbeError {
                    status: None,
                    message: "error sending request: connection refused".into(),
                    retry_after_secs: None,
                })
            })
        }
    }

    let manager = build_manager_with_prober(
        config_with(vec![account("a", 0)]),
        counting_refresher(),
        Arc::new(NetFailProber),
    );
    manager.probe_all().await;
    let snap = manager.snapshot(OffsetDateTime::now_utc());
    assert_eq!(
        snap.accounts[0].probe_status,
        ProbeStatus::Error,
        "a transport failure must stay a visible error, not a soft busy"
    );
}
