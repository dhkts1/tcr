//! Codex rows beside Anthropic ones: refusals, weekly windows, provider routing, traffic rings.

use super::*;

/// **The folder rides the wire on Codex rows only.** A Codex row imported from a folder
/// echoes it; an Anthropic row has none to echo.
///
/// Watched red: without the copy in `from_config`, the Codex row echoes nothing.
#[test]
fn the_snapshot_echoes_codex_home_on_codex_rows_only() {
    let codex = Account {
        codex_home: Some("/Users/example/.codex".to_string()),
        ..codex_account("bob@example.com", 1)
    };
    let manager = manager_of(vec![account("alice@example.com", 0), codex]);
    let snap = manager.snapshot(OffsetDateTime::now_utc());
    assert_eq!(snap.accounts[0].codex.codex_home, None);
    assert_eq!(
        snap.accounts[1].codex.codex_home.as_deref(),
        Some("/Users/example/.codex")
    );
}

/// **A login revives a deleted Codex row with its new folder and no stale error.** A row
/// deleted while the proxy ran keeps its slot, parked; a login of the same identity brings
/// it back through `add_or_update_account`'s Updated arm, and the durable write appends the
/// whole row again, because the file has no entry left to update. Both of the row's Codex
/// facts must come from that login: no `login_error` (the login is live), and the
/// submission's folder, in the running proxy and in the file.
///
/// Watched red twice: without the `login_error` clear, the revived row still reads
/// `refresh_token_reused`; without `codex_home` in the persisted row, the file's entry has
/// no `codexHome`.
#[tokio::test]
async fn a_relogin_of_a_deleted_codex_row_revives_it_with_its_folder_and_no_login_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(crate::paths::CONFIG_FILE);
    let alice = account("alice@example.com", 0);
    let bob = Account {
        codex_home: Some("/Users/example/.codex".to_string()),
        ..codex_account("bob@example.com", 1)
    };
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&serde_json::json!({ "accounts": [alice.clone()] }))
            .expect("serialize"),
    )
    .expect("write the file");
    let manager = Manager::new(
        config_with(vec![alice, bob.clone()]),
        Arc::new(RejectsWith {
            status: 400,
            code: Some("refresh_token_reused"),
        }),
        Arc::new(NoProber),
        Arc::new(NoWarmer),
        Some(path.clone()),
    );

    manager.ensure_fresh_force(1).await;
    let before = manager.snapshot(OffsetDateTime::now_utc());
    assert!(
        before.accounts.iter().all(|a| a.name != "bob@example.com"),
        "the file lacks bob, so the reload took him out"
    );

    let relogin = Account {
        access_token: "at-bob-fresh".to_string(),
        refresh_token: Some("rt-bob-fresh".to_string()),
        codex_home: Some("/Users/example/.codex-b".to_string()),
        ..bob
    };
    assert!(matches!(
        manager.add_or_update_account(relogin),
        AddAccountOutcome::Updated { idx: 1, .. }
    ));

    let after = manager.snapshot(OffsetDateTime::now_utc());
    let bobs: Vec<_> = after
        .accounts
        .iter()
        .filter(|a| a.name == "bob@example.com")
        .collect();
    assert_eq!(bobs.len(), 1, "bob is listed once");
    assert!(!bobs[0].disabled, "a fresh login serves");
    assert_eq!(bobs[0].codex.login_error, None);
    assert_eq!(
        bobs[0].codex.codex_home.as_deref(),
        Some("/Users/example/.codex-b")
    );
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("json");
    let entry = doc["accounts"]
        .as_array()
        .expect("accounts")
        .iter()
        .find(|a| a["name"] == "bob@example.com")
        .expect("bob is back in the file");
    assert_eq!(entry["codexHome"], "/Users/example/.codex-b");
}

/// **A Codex account is never asked the Anthropic profile endpoint for its plan.** That
/// endpoint is the backfill's only source, so asking it for a Codex row hands an OpenAI
/// bearer to the wrong provider; and since no plan ever comes back, the once-per-account
/// gate never closes and it would be asked again on every probe. The Anthropic row in
/// the same fleet is still asked, so the skip is the provider and not a backfill that
/// stopped running.
///
/// Watched red: drop the Codex early return in `backfill_plan_if_unknown` and the plan
/// prober is asked for the Codex row.
#[tokio::test]
async fn a_codex_account_is_never_asked_the_anthropic_profile_for_its_plan() {
    let codex = Account {
        provider: crate::provider::Provider::Codex,
        ..account("bob@example.com", 1)
    };
    let manager = build_manager_with_prober(
        config_with(vec![account("alice@example.com", 0), codex]),
        counting_refresher(),
        Arc::new(NoProber),
    );
    let calls = count_plan_fetches(&manager, crate::probe::Plan::default());

    manager.probe_account(1).await;
    assert_eq!(
        calls.load(Ordering::Relaxed),
        0,
        "the Codex row must never reach the Anthropic profile endpoint"
    );

    manager.probe_account(0).await;
    assert_eq!(
        calls.load(Ordering::Relaxed),
        1,
        "control: the Anthropic row is still asked"
    );
}

/// **A Codex `rejected` is lifted by a probe that sees its window roll**, the path an
/// Anthropic rejection already takes ([`crate::quota::Quota::apply_usage`]). A Codex
/// quota refusal, a 429 carrying `x-codex-rate-limit-reached-type`, marks the account
/// `rejected`; `select()` treats that as terminal, so no served response comes back to
/// clear it, and before the Codex probe it stood until the process restarted.
///
/// End to end from the production constructor: `with_live_refresher` points the live
/// prober at the configured `codexUpstream` (a local stub here), the Codex arm sends the
/// row's bearer and `chatgpt-account-id`, and the measured weekly-only payload (the
/// weekly window in the PRIMARY slot, at 2% once it has rolled) lands in `seven_day`,
/// the window the refusal left at 100%.
///
/// The prober's Codex base is checked before anything probes, so a regressed wiring
/// fails here, offline, instead of sending the fixture bearer to the real backend.
///
/// Watched red, one break at a time: drop the `with_codex_base` chain from
/// `with_live_refresher` and the prober's base is the real backend's, caught before any
/// probe; map the payload by position and the 2% lands in `five_hour`, the stored weekly
/// 100% never falls, and the account stays rejected.
#[tokio::test]
async fn a_codex_rejection_is_lifted_when_a_probe_sees_its_weekly_window_roll() {
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&seen);
    let usage_endpoint = axum::Router::new().route(
        crate::provider::CODEX_USAGE_PATH,
        axum::routing::get(move |headers: axum::http::HeaderMap| {
            let recorder = Arc::clone(&recorder);
            async move {
                let header = |name: &str| {
                    headers
                        .get(name)
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string)
                };
                recorder
                    .lock()
                    .expect("recorder lock poisoned")
                    .push((header("authorization"), header("chatgpt-account-id")));
                axum::Json(serde_json::json!({
                    "plan_type": "pro",
                    "rate_limit": {
                        "allowed": true,
                        "limit_reached": false,
                        "primary_window": {
                            "used_percent": 2,
                            "limit_window_seconds": 604_800,
                            "reset_after_seconds": 604_000,
                            "reset_at": now + 604_000,
                        },
                        "secondary_window": null,
                    },
                }))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the usage stub");
    let codex_upstream = format!("http://{}", listener.local_addr().expect("stub address"));
    tokio::spawn(async move {
        crate::log_err::LogErr::log_debug(
            axum::serve(listener, usage_endpoint).await,
            "serving the usage stub",
        );
    });

    let mut config = config_with(vec![Account {
        provider: crate::provider::Provider::Codex,
        account_uuid: Some("22222222-2222-4222-8222-222222222222".to_string()),
        ..account("bob@example.com", 0)
    }]);
    config.codex_upstream = codex_upstream.clone();
    let manager = Manager::with_live_refresher(config, None);
    assert_eq!(
        manager.prober.codex_base(),
        Some(codex_upstream.as_str()),
        "the live prober must probe Codex at the configured codexUpstream"
    );
    // The backfill skips a Codex row; should that ever break, this keeps the live
    // profile endpoint out of the test.
    count_plan_fetches(&manager, crate::probe::Plan::default());
    let codex_request = || {
        manager.select(
            &HashSet::new(),
            OffsetDateTime::now_utc(),
            None,
            None,
            crate::provider::CODEX_RESPONSES_PATH,
            None,
        )
    };

    // The refusal: the weekly window spent, reported in the primary slot.
    let mut refusal = reqwest::header::HeaderMap::new();
    refusal.insert("x-codex-primary-used-percent", "100".parse().unwrap());
    refusal.insert("x-codex-primary-window-minutes", "10080".parse().unwrap());
    refusal.insert(
        "x-codex-primary-reset-at",
        (now + 3600).to_string().parse().unwrap(),
    );
    refusal.insert(
        crate::quota::CODEX_REACHED_TYPE_HEADER,
        "primary".parse().unwrap(),
    );
    manager.update_quota(0, &refusal, 429);
    assert_eq!(
        manager.accounts.read().expect("accounts lock poisoned")[0]
            .quota
            .status
            .as_deref(),
        Some("rejected"),
        "precondition: the refusal marked the account rejected"
    );
    assert_eq!(
        codex_request(),
        None,
        "precondition: a rejected account serves nothing"
    );

    manager.probe_account(0).await;

    assert_eq!(
        seen.lock().expect("recorder lock poisoned").clone(),
        vec![(
            Some("Bearer at-bob@example.com".to_string()),
            Some("22222222-2222-4222-8222-222222222222".to_string()),
        )],
        "one probe, at the configured upstream, with the row's bearer and account id"
    );
    let account = manager.accounts.read().expect("accounts lock poisoned")[0].clone();
    assert_eq!(
        account.quota.status, None,
        "the weekly window rolled, so the rejection recorded against it is stale"
    );
    assert_eq!(
        codex_request(),
        Some(0),
        "and the account is back in rotation"
    );
    let weekly = account.quota.seven_day.expect("the probe's weekly reading");
    assert!(
        (weekly.utilization - 0.02).abs() < 1e-9,
        "the probe's 2% replaced the refusal's 100%, got {}",
        weekly.utilization
    );
    assert!(
        account.quota.five_hour.is_none(),
        "the payload carried no five-hour window, so none may be invented"
    );
}

/// A manager over `alice@example.com` at priority 0, then one Codex row
/// `<name>@example.com/codex` per name, at priorities 1, 2, and on.
fn alice_then_codex_rows(names: &[&str]) -> Arc<Manager> {
    let codex = names.iter().zip(1..).map(|(name, priority)| Account {
        provider: crate::provider::Provider::Codex,
        ..account(&format!("{name}@example.com/codex"), priority)
    });
    manager_of(
        std::iter::once(account("alice@example.com", 0))
            .chain(codex)
            .collect(),
    )
}

/// Codex quota headers saying the weekly window (the only one the measured plan
/// reports) is `used` percent spent, resetting a day out.
fn codex_weekly(used: &str) -> reqwest::header::HeaderMap {
    let reset = OffsetDateTime::now_utc().unix_timestamp() + 86_400;
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("x-codex-primary-used-percent", used.parse().unwrap());
    headers.insert("x-codex-primary-window-minutes", "10080".parse().unwrap());
    headers.insert(
        "x-codex-primary-reset-at",
        reset.to_string().parse().unwrap(),
    );
    headers
}

/// **A Codex row's `quotaUpdatedAtMs` comes from that row's own readings**, both
/// kinds: a served response's Codex quota headers, which `update_quota` routes to
/// the Codex parser by the row's provider, and a successful Codex probe. The
/// Anthropic row beside it read nothing and stays absent, and a Codex response
/// that carried no quota header leaves the time where it was. Walked to the
/// `tcr.status.v1` payload, the hop `tcr status --json` reads.
///
/// Watched red: without the stamp in `Quota::update_from_codex_refusal_at`, the
/// header reading leaves the Codex row absent.
#[tokio::test]
async fn a_codex_rows_quota_updated_at_comes_from_its_own_readings() {
    let codex = Account {
        provider: crate::provider::Provider::Codex,
        account_uuid: Some("22222222-2222-4222-8222-222222222222".to_string()),
        ..account("bob@example.com/codex", 1)
    };
    let manager = build_manager_with_prober(
        config_with(vec![account("alice@example.com", 0), codex]),
        counting_refresher(),
        Arc::new(RecordingProber {
            seen: Arc::new(Mutex::new(Vec::new())),
        }),
    );
    let wire_value = quota_updated_at_on_the_wire;
    let stale = crate::now_ms() - 40 * 60 * 1000;
    let rewind = |manager: &Manager| {
        accounts_mut(manager)[1].quota.updated_at = Some(
            OffsetDateTime::from_unix_timestamp_nanos(i128::from(stale) * 1_000_000)
                .expect("valid instant"),
        );
    };
    assert_eq!(wire_value(&manager, 1), None, "nothing read yet");

    let before = crate::now_ms();
    manager.update_quota(1, &codex_weekly("40"), 200);
    let read = wire_value(&manager, 1).expect("a Codex header reading carries quotaUpdatedAtMs");
    assert!(
        read >= before,
        "a fresh Codex reading reads as recent: {read} < {before}"
    );
    assert_eq!(
        wire_value(&manager, 0),
        None,
        "the Anthropic row read nothing"
    );

    rewind(&manager);
    manager.update_quota(1, &reqwest::header::HeaderMap::new(), 502);
    assert_eq!(
        wire_value(&manager, 1),
        Some(stale),
        "silence is not a reading"
    );

    rewind(&manager);
    let before = crate::now_ms();
    manager.probe_account(1).await;
    let probed = wire_value(&manager, 1).expect("still present");
    assert!(
        probed >= before,
        "a successful Codex probe moves it: {probed} < {before}"
    );
}

/// **#466's shorter probe cadence reads a Codex row by its own readings.** A Codex
/// account whose weekly window is at its threshold, or that a Codex quota refusal
/// marked rejected below it, is out of rotation, so no served response refreshes it
/// and only a probe sees its window roll: it gets the near-limit cadence. An idle
/// Codex account keeps the routine one, as does the Anthropic account that read
/// nothing. The probe that cadence fires is the row's own provider's
/// (`a_probe_credential_names_the_accounts_provider_and_account_uuid` pins the
/// credential), and the hourly cap counts each account apart, so a faster Codex
/// cadence sends Codex bearers to `wham/usage` and nothing to Anthropic.
///
/// Watched red: read every Codex row as routine in `ProbeUrgency::of`, and bob and
/// carol lose the shorter cadence.
#[test]
fn a_held_codex_account_gets_the_near_limit_probe_cadence() {
    use crate::schedule::ProbeUrgency;
    let manager = alice_then_codex_rows(&["bob", "carol", "dave"]);
    // bob: at its weekly limit, read from a served 200.
    manager.update_quota(1, &codex_weekly("97"), 200);
    // carol: a Codex quota refusal at half its weekly window.
    let mut refusal = codex_weekly("50");
    refusal.insert(
        crate::quota::CODEX_REACHED_TYPE_HEADER,
        "primary".parse().unwrap(),
    );
    manager.update_quota(2, &refusal, 429);
    // dave: idle.
    manager.update_quota(3, &codex_weekly("10"), 200);

    let snap = manager.snapshot(OffsetDateTime::now_utc());
    let urgency: Vec<(&str, ProbeUrgency)> = snap
        .accounts
        .iter()
        .map(|a| (a.name.as_str(), ProbeUrgency::of(a)))
        .collect();
    assert_eq!(
        urgency,
        vec![
            ("alice@example.com", ProbeUrgency::Routine),
            ("bob@example.com/codex", ProbeUrgency::NearLimit),
            ("carol@example.com/codex", ProbeUrgency::NearLimit),
            ("dave@example.com/codex", ProbeUrgency::Routine),
        ]
    );
    assert_eq!(
        (snap.accounts[2].gate, snap.accounts[2].quota_state),
        (GateReason::Rejected, crate::stats::QuotaState::Normal),
        "control: carol is held by its refusal, not by its threshold"
    );
}

/// **A Codex refusal keeps the near-limit probe cadence whatever gate its row reports.**
/// The snapshot reports a standing refusal as the window or the hold that explains it
/// (`Manager::codex_refusal_gate`), not as `Rejected`, and `ProbeUrgency::of` reads that
/// row. The refusal still stands until a probe ends it, so the account keeps the cadence a
/// `rejected` one gets: while the hold the proxy armed for the refusal lasts (`Hold`),
/// while an upstream 403's hold lasts (`Refused`), and past the refusing window's reset
/// before a probe confirms the roll (that window, with no time). Every one of them reads
/// below its threshold at that moment, so only the refusal can set the cadence. A hold no
/// refusal set stays routine, as does the Anthropic account, and so does a refused account
/// whose login then died: `Login` outranks `Rejected`, so the refusal never was what held
/// it out, and before the remap it read routine too.
///
/// Watched red: read only `GateReason::Rejected` in `ProbeUrgency::of`, and bob, carol and
/// erin read routine; read any standing refusal whatever the gate, and frank reads
/// near-limit.
#[test]
fn a_codex_refusal_keeps_the_near_limit_cadence_whatever_gate_its_row_reports() {
    use crate::schedule::ProbeUrgency;
    use crate::stats::{GateReason, QuotaState};
    let manager = alice_then_codex_rows(&["bob", "carol", "dave", "erin", "frank"]);
    let refusal = |mut headers: reqwest::header::HeaderMap| {
        headers.insert(
            crate::quota::CODEX_REACHED_TYPE_HEADER,
            "rate_limit_reached".parse().unwrap(),
        );
        headers
    };
    // bob: refused at half its weekly window, held as the proxy holds every refusal.
    manager.update_quota(1, &refusal(codex_weekly("50")), 429);
    manager.mark_rate_limited(1, 1800);
    // carol: refused on a full weekly window that rolls in half a minute, read a minute on,
    // before any probe.
    let soon = OffsetDateTime::now_utc().unix_timestamp() + 30;
    let mut full = reqwest::header::HeaderMap::new();
    full.insert("x-codex-primary-used-percent", "100".parse().unwrap());
    full.insert("x-codex-primary-window-minutes", "10080".parse().unwrap());
    full.insert(
        "x-codex-primary-reset-at",
        soon.to_string().parse().unwrap(),
    );
    manager.update_quota(2, &refusal(full), 429);
    // dave: a hold no refusal set, over an idle weekly window.
    manager.update_quota(3, &codex_weekly("10"), 200);
    manager.mark_rate_limited(3, 1800);
    // erin: refused at half its weekly window, then held by an upstream 403.
    manager.update_quota(4, &refusal(codex_weekly("50")), 429);
    manager.mark_refused(4);
    // frank: refused at half its weekly window, then its login died.
    manager.update_quota(5, &refusal(codex_weekly("50")), 429);
    accounts_mut(&manager)[5].status = AccountStatus::Error;

    // Past carol's reset, well inside erin's two-minute hold.
    let later = OffsetDateTime::now_utc() + Duration::minutes(1);
    let snap = manager.snapshot(later);
    let rows: Vec<(&str, GateReason, QuotaState, ProbeUrgency)> = snap
        .accounts
        .iter()
        .map(|a| (a.name.as_str(), a.gate, a.quota_state, ProbeUrgency::of(a)))
        .collect();
    assert_eq!(
        rows,
        vec![
            (
                "alice@example.com",
                GateReason::Ok,
                QuotaState::Normal,
                ProbeUrgency::Routine
            ),
            (
                "bob@example.com/codex",
                GateReason::Hold,
                QuotaState::Normal,
                ProbeUrgency::NearLimit
            ),
            (
                "carol@example.com/codex",
                GateReason::SevenDay,
                QuotaState::Normal,
                ProbeUrgency::NearLimit
            ),
            (
                "dave@example.com/codex",
                GateReason::Hold,
                QuotaState::Normal,
                ProbeUrgency::Routine
            ),
            (
                "erin@example.com/codex",
                GateReason::Refused,
                QuotaState::Normal,
                ProbeUrgency::NearLimit
            ),
            (
                "frank@example.com/codex",
                GateReason::Login,
                QuotaState::Normal,
                ProbeUrgency::Routine
            ),
        ]
    );
}

/// **A Codex row's snapshot reports its quota facts, and the refusal's kind exactly while
/// that refusal stands.** The refusal sets the kind, the credits and which windows the plan
/// has; a probe that sees the window roll ends the refusal and its kind; a new refusal
/// brings a new kind, which a probe refreshes while it stands, and that probe, carrying no
/// window, reports `[]`: the plan has none, which is a fact, not an unknown; a 2xx that
/// carried a window ends it, and a probe then never brings the kind back. An Anthropic row
/// reports none of it, even with a stray fact on its quota.
///
/// Watched red, one break at a time: fill the facts for every row and the Anthropic row
/// reports the stray credits; let the accessor ignore the status and drop the clear on a
/// 2xx, and the kind outlives the refusal; filter an empty list out in
/// `codex_snapshot_facts`, and the `[]` reads `None`.
#[test]
fn the_reached_type_is_present_exactly_while_the_refusal_stands() {
    use crate::codex_probe::CodexProbeFacts;
    use tcr_status_wire::CodexCredits;
    let manager = manager_of(vec![
        Account {
            provider: crate::provider::Provider::Codex,
            account_uuid: Some("22222222-2222-4222-8222-222222222222".into()),
            ..account("bob@example.com", 0)
        },
        account("alice@example.com", 1),
    ]);
    let snap = |idx: usize| manager.snapshot(OffsetDateTime::now_utc()).accounts[idx].clone();
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let now_ms = crate::now_ms();
    let headers = |pairs: &[(&str, String)]| {
        let mut h = reqwest::header::HeaderMap::new();
        for (name, value) in pairs {
            h.insert(
                reqwest::header::HeaderName::from_bytes(name.as_bytes()).expect("a name"),
                value.parse().expect("a header value"),
            );
        }
        h
    };
    let credits = Some(CodexCredits {
        has_credits: true,
        unlimited: false,
        balance: Some("12.50".into()),
    });

    // 1. The weekly refusal.
    let refusal = headers(&[
        ("x-codex-primary-used-percent", "100".into()),
        ("x-codex-primary-window-minutes", "10080".into()),
        ("x-codex-primary-reset-at", (now + 3 * 86_400).to_string()),
        (
            crate::quota::CODEX_REACHED_TYPE_HEADER,
            "rate_limit_reached".into(),
        ),
        ("x-codex-credits-has-credits", "true".into()),
        ("x-codex-credits-unlimited", "false".into()),
        ("x-codex-credits-balance", "12.50".into()),
    ]);
    manager.update_quota(0, &refusal, 429);
    let bob = snap(0).codex;
    assert_eq!(
        bob.limit_reached_type.as_deref(),
        Some("rate_limit_reached")
    );
    assert_eq!(bob.quota_windows, Some(vec!["sevenDay".to_string()]));
    assert_eq!(bob.seven_day_window_minutes, Some(10_080));
    assert_eq!(bob.five_hour_window_minutes, None);
    assert_eq!(bob.credits, credits);
    assert!(!bob.spend_control_reached);

    // 2. A probe sees the weekly window roll: the refusal ends, and its kind with it.
    manager.apply_usage(
        0,
        &Usage {
            seven_day: Some(UsageBucket {
                utilization: Some(0.02),
                reset_at_ms: Some(now_ms + 604_000_000),
            }),
            ..Usage::default()
        },
    );
    assert_eq!(snap(0).codex.limit_reached_type, None, "the window rolled");

    // 3. A refusal no full window explains brings its own kind.
    let refusal_no_full_window = headers(&[
        ("x-codex-primary-used-percent", "40".into()),
        ("x-codex-primary-window-minutes", "10080".into()),
        ("x-codex-primary-reset-at", (now + 3 * 86_400).to_string()),
        (
            crate::quota::CODEX_REACHED_TYPE_HEADER,
            "workspace_credits".into(),
        ),
    ]);
    manager.update_quota(0, &refusal_no_full_window, 429);
    assert_eq!(
        snap(0).codex.limit_reached_type.as_deref(),
        Some("workspace_credits")
    );

    // 4. While it stands, a probe's kind refreshes it.
    let probe_kind = Usage {
        codex: Some(CodexProbeFacts {
            limit_reached_type: Some("rate_limit_reached".into()),
            ..Default::default()
        }),
        ..Usage::default()
    };
    manager.apply_usage(0, &probe_kind);
    assert_eq!(
        snap(0).codex.limit_reached_type.as_deref(),
        Some("rate_limit_reached"),
        "the probe refreshes the kind while the refusal stands"
    );
    assert_eq!(
        snap(0).codex.quota_windows,
        Some(Vec::new()),
        "a probe that carried no window says the plan reports none: [], not unknown"
    );

    // 5. A 2xx that carried a window ends the refusal; a probe never brings its kind back.
    let ok_with_window = headers(&[
        ("x-codex-primary-used-percent", "41".into()),
        ("x-codex-primary-window-minutes", "10080".into()),
        ("x-codex-primary-reset-at", (now + 3 * 86_400).to_string()),
    ]);
    manager.update_quota(0, &ok_with_window, 200);
    assert_eq!(snap(0).codex.limit_reached_type, None, "the 2xx ended it");
    manager.apply_usage(0, &probe_kind);
    assert_eq!(
        snap(0).codex.limit_reached_type,
        None,
        "a probe never brings the kind back without a refusal"
    );

    // 6. Control: the Anthropic row reports none of it, even with a stray fact.
    let anthropic_ok = headers(&[("anthropic-ratelimit-unified-5h-utilization", "0.20".into())]);
    manager.update_quota(1, &anthropic_ok, 200);
    assert_eq!(snap(1).codex, crate::stats::CodexFacts::default());
    manager.accounts.write().expect("accounts lock poisoned")[1]
        .quota
        .codex
        .credits = Some(CodexCredits::default());
    assert_eq!(
        snap(1).codex,
        crate::stats::CodexFacts::default(),
        "only a Codex row ever reports these facts"
    );
}

/// **A Codex refusal ends when a usage probe reads the account allowed**: allowed, not at
/// its limit, its spend control not reached, and no refusal kind. That is the backend's own
/// verdict, so a probe ends a refusal no window explains (a spend cap, credits) once ChatGPT
/// ends it. Each condition is needed: a probe that reads any one of them otherwise, or that
/// carries no `rate_limit` at all, leaves the refusal and its kind standing. The window-roll
/// lift still ends a refusal on a probe that says not allowed, and an Anthropic rejection is
/// untouched: its probe carries no verdict.
///
/// Driven through the real payload parser (`codex_probe::usage_from_payload`), each probe at
/// the refusal's own 60 percent, so no window rolls.
///
/// Watched red, one condition dropped at a time: without `allowed`, the probe carrying no
/// `rate_limit` lifts the refusal; without `limit_reached`, the probe allowed but at its
/// limit does; without the spend control, the spend-control probe does; without the kind,
/// the probe carrying one does.
#[test]
fn a_codex_refusal_clears_when_a_probe_reads_it_allowed() {
    use crate::codex_probe::usage_from_payload;
    use serde_json::{Value, json};
    let manager = manager_of(vec![
        Account {
            provider: crate::provider::Provider::Codex,
            account_uuid: Some("22222222-2222-4222-8222-222222222222".into()),
            ..account("bob@example.com", 0)
        },
        account("alice@example.com", 1),
    ]);
    let gate = |idx: usize| {
        let row = manager.snapshot(OffsetDateTime::now_utc()).accounts[idx].clone();
        (row.gate, row.codex.limit_reached_type)
    };
    let codex_request = || {
        manager.select(
            &HashSet::new(),
            OffsetDateTime::now_utc(),
            None,
            None,
            crate::provider::CODEX_RESPONSES_PATH,
            None,
        )
    };
    let reset_at = (OffsetDateTime::now_utc() + Duration::days(5)).unix_timestamp();
    // The Pro refusal: its one window, the weekly one, at 60 percent in the primary slot.
    let refuse = || {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert("x-codex-primary-used-percent", "60".parse().unwrap());
        h.insert("x-codex-primary-window-minutes", "10080".parse().unwrap());
        h.insert(
            "x-codex-primary-reset-at",
            reset_at.to_string().parse().unwrap(),
        );
        h.insert(
            crate::quota::CODEX_REACHED_TYPE_HEADER,
            "rate_limit_reached".parse().unwrap(),
        );
        manager.update_quota(0, &h, 429);
    };
    let payload = |allowed: bool, limit_reached: bool, spend: bool, kind: Value, used: u32| {
        json!({
            "plan_type": "pro",
            "rate_limit": {
                "allowed": allowed,
                "limit_reached": limit_reached,
                "primary_window": {
                    "used_percent": used,
                    "limit_window_seconds": 604_800,
                    "reset_at": reset_at,
                },
                "secondary_window": null,
            },
            "spend_control": { "reached": spend },
            "rate_limit_reached_type": kind,
        })
    };
    let refused = (GateReason::Rejected, Some("rate_limit_reached".to_string()));

    refuse();
    assert_eq!(gate(0), refused, "precondition: refused, with its kind");

    let kind = json!({ "type": "rate_limit_reached", "details": "fixture" });
    for (case, probe) in [
        (
            "not allowed, at its limit",
            payload(false, true, false, Value::Null, 60),
        ),
        (
            "allowed but at its limit",
            payload(true, true, false, Value::Null, 60),
        ),
        (
            "spend control reached",
            payload(true, false, true, Value::Null, 60),
        ),
        (
            "carrying a refusal kind",
            payload(true, false, false, kind, 60),
        ),
        (
            "no rate_limit",
            json!({
                "plan_type": "pro",
                "rate_limit": null,
                "spend_control": { "reached": false },
            }),
        ),
    ] {
        manager.apply_usage(0, &usage_from_payload(&probe));
        assert_eq!(gate(0), refused, "{case}: the refusal and its kind stand");
    }

    manager.apply_usage(
        0,
        &usage_from_payload(&payload(true, false, false, Value::Null, 60)),
    );
    assert_eq!(
        gate(0),
        (GateReason::Ok, None),
        "read allowed: the refusal and its kind end"
    );
    // The pool pick (`Manager::eligible`) reads the hold and the windows, never `rejected`,
    // so this holds before the lift too: what the lift ends is the refusal itself, its kind
    // and the gate every reader is shown, and with them the pin and control gates that read
    // `rejected` (`account_terminal_gate`).
    assert_eq!(codex_request(), Some(0), "and the account serves");

    // The window-roll lift, on a probe that says not allowed: the weekly reading fell.
    refuse();
    assert_eq!(gate(0), refused, "a fresh refusal");
    manager.apply_usage(
        0,
        &usage_from_payload(&payload(false, false, false, Value::Null, 2)),
    );
    assert_eq!(gate(0), (GateReason::Ok, None), "the window rolled");

    // Control: an Anthropic rejection, probed at the same reading, stands.
    {
        let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
        accounts[1].quota.status = Some("rejected".to_string());
        accounts[1].quota.seven_day = Some(crate::quota::QuotaWindow {
            utilization: 0.6,
            reset: Some(OffsetDateTime::now_utc() + Duration::days(5)),
        });
    }
    manager.apply_usage(
        1,
        &Usage {
            seven_day: Some(UsageBucket {
                utilization: Some(0.6),
                reset_at_ms: Some(reset_at * 1000),
            }),
            ..Usage::default()
        },
    );
    assert_eq!(
        gate(1).0,
        GateReason::Rejected,
        "an Anthropic probe lifts nothing"
    );
}

#[test]
fn upstream_for_routes_each_provider_to_its_own_host() {
    // Built the way the other tests here build a manager, from a Config whose `upstream` and
    // `codex_upstream` name two different hosts.
    let mut config = config_with(vec![account("alice@example.com", 0)]);
    config.upstream = "http://anthropic.test".to_string();
    config.codex_upstream = "http://codex.test".to_string();
    let manager = build_manager(config, counting_refresher());
    assert_eq!(
        manager.upstream_for(crate::provider::Provider::Anthropic),
        "http://anthropic.test"
    );
    assert_eq!(
        manager.upstream_for(crate::provider::Provider::Codex),
        "http://codex.test"
    );
    assert_eq!(
        manager.upstream(),
        "http://anthropic.test",
        "the Anthropic-only accessor is unchanged"
    );
}

/// `Manager::provider` reads each runtime row's own provider, which
/// `AccountRuntime::from_config` copied from the config row, and `None`
/// only past the end.
///
/// Watched red: have the accessor answer the default for every row and the
/// Codex row reads as Anthropic.
#[test]
fn provider_reads_each_rows_own_provider() {
    let codex = Account {
        provider: crate::provider::Provider::Codex,
        ..account("bob@example.com", 0)
    };
    let manager = manager_of(vec![account("alice@example.com", 0), codex]);
    assert_eq!(
        manager.provider(0),
        Some(crate::provider::Provider::Anthropic)
    );
    assert_eq!(manager.provider(1), Some(crate::provider::Provider::Codex));
    assert_eq!(manager.provider(2), None, "an index past the end");
}

/// `account` as a Codex account, told from the others by its name alone (no workspace or
/// user id, unlike [`codex_account`], whose rows all share one login).
fn codex_account_named(name: &str) -> Account {
    Account {
        provider: crate::provider::Provider::Codex,
        ..account(name, 0)
    }
}

/// Delete every account but `kept` from the config at `path` while `manager` runs, the way
/// `tcr remove` or a hand edit does, and take the snapshot that notices. The sleep moves the
/// file's mtime, which is what the reload watches.
fn delete_from_disk(
    manager: &Manager,
    path: &std::path::Path,
    kept: Vec<Account>,
) -> StatsSnapshot {
    std::thread::sleep(std::time::Duration::from_millis(20));
    config::save(path, &config_with(kept)).expect("rewrite the test config");
    manager.snapshot(OffsetDateTime::now_utc())
}

fn names(snapshot: &StatsSnapshot) -> Vec<&str> {
    snapshot.accounts.iter().map(|a| a.name.as_str()).collect()
}

fn slots(snapshot: &StatsSnapshot) -> Vec<usize> {
    snapshot.accounts.iter().map(|a| a.slot).collect()
}

/// The dashboard marks the account serving EACH provider, so the manager remembers one per
/// provider: a Codex request served after a Claude one must not erase which account is
/// serving Claude. `current` keeps its meaning, the account that served last of either.
#[test]
fn the_serving_account_is_tracked_per_provider() {
    let (manager, path) = build_manager_with_disk(
        config_with(vec![
            account("alice@example.com", 0),
            codex_account_named("bob@example.com/codex"),
        ]),
        "serving-per-provider",
    );
    manager.set_current(0);
    manager.set_current(1);

    let snapshot = manager.snapshot(OffsetDateTime::now_utc());
    assert_eq!(
        snapshot.current_by_provider,
        crate::stats::CurrentByProvider {
            anthropic: Some(0),
            codex: Some(1),
        }
    );
    assert_eq!(snapshot.current, Some(1));
    std::fs::remove_file(&path).ok();
}

/// The serving accounts are positions in the snapshot's list, as `current` is, so a row
/// deleted above one moves it up by one, and a deleted serving row reads as nothing serving.
/// A slot is never what the list is indexed by: after the first delete, carol is slot 2 and
/// position 1.
#[test]
fn the_serving_account_survives_a_deleted_row_above_it() {
    let alice = account("alice@example.com", 0);
    let carol = codex_account_named("carol@example.com/codex");
    let (manager, path) = build_manager_with_disk(
        config_with(vec![
            alice.clone(),
            codex_account_named("bob@example.com/codex"),
            carol.clone(),
        ]),
        "serving-past-a-delete",
    );
    manager.set_current(0);
    manager.set_current(2);
    let before = manager.snapshot(OffsetDateTime::now_utc());
    assert_eq!(slots(&before), [0, 1, 2]);
    assert_eq!(
        before.current_by_provider,
        crate::stats::CurrentByProvider {
            anthropic: Some(0),
            codex: Some(2),
        }
    );

    let without_bob = delete_from_disk(&manager, &path, vec![alice.clone(), carol]);
    // First, that the reload happened at all: a file it could not parse would keep every
    // row, and the assertions below would pass or fail for that reason instead.
    assert_eq!(
        names(&without_bob),
        ["alice@example.com", "carol@example.com/codex"]
    );
    assert_eq!(slots(&without_bob), [0, 2]);
    assert_eq!(
        without_bob.current_by_provider,
        crate::stats::CurrentByProvider {
            anthropic: Some(0),
            codex: Some(1),
        }
    );
    assert_eq!(without_bob.current, Some(1));

    let without_carol = delete_from_disk(&manager, &path, vec![alice]);
    assert_eq!(names(&without_carol), ["alice@example.com"]);
    assert_eq!(
        without_carol.current_by_provider,
        crate::stats::CurrentByProvider {
            anthropic: Some(0),
            codex: None,
        }
    );
    assert_eq!(without_carol.current, None);
    std::fs::remove_file(&path).ok();
}

/// A session carries the provider of the account it is pinned to, and a log line the
/// provider it was pushed with. A session pinned to a row deleted from the config keeps
/// that row's provider, as it keeps that row's name: the pin still points at the slot.
#[test]
fn sessions_and_log_entries_carry_their_provider() {
    let alice = account("alice@example.com", 0);
    let (manager, path) = build_manager_with_disk(
        config_with(vec![
            alice.clone(),
            codex_account_named("bob@example.com/codex"),
        ]),
        "session-provider",
    );
    let now_ms = crate::now_ms();
    manager.seed_session(0xA11CE, 0, 3, now_ms, SessionKind::Stable);
    pin_session(&manager, 0xA11CE, 0);
    manager.seed_session(0xB0B, 1, 5, now_ms, SessionKind::Stable);
    pin_session(&manager, 0xB0B, 1);
    for (path, provider) in [
        ("/v1/messages", crate::provider::Provider::Anthropic),
        (
            crate::provider::CODEX_RESPONSES_PATH,
            crate::provider::Provider::Codex,
        ),
    ] {
        manager.push_log(RequestLogEntry {
            time: OffsetDateTime::now_utc(),
            method: "POST".to_string(),
            path: path.to_string(),
            status: 200,
            account: String::new(),
            provider,
        });
    }

    let provider_of = |snapshot: &StatsSnapshot, account: &str| {
        snapshot
            .sessions
            .iter()
            .find(|s| s.account == account)
            .unwrap_or_else(|| panic!("no session pinned to {account}: {:?}", snapshot.sessions))
            .provider
    };
    let snapshot = manager.snapshot(OffsetDateTime::now_utc());
    assert_eq!(
        provider_of(&snapshot, "alice@example.com"),
        crate::provider::Provider::Anthropic
    );
    assert_eq!(
        provider_of(&snapshot, "bob@example.com/codex"),
        crate::provider::Provider::Codex
    );
    let logged: Vec<(&str, crate::provider::Provider)> = snapshot
        .recent
        .iter()
        .map(|e| (e.path.as_str(), e.provider))
        .collect();
    assert_eq!(
        logged,
        [
            (
                crate::provider::CODEX_RESPONSES_PATH,
                crate::provider::Provider::Codex
            ),
            ("/v1/messages", crate::provider::Provider::Anthropic),
        ]
    );

    let without_bob = delete_from_disk(&manager, &path, vec![alice]);
    assert_eq!(names(&without_bob), ["alice@example.com"]);
    assert_eq!(
        provider_of(&without_bob, "bob@example.com/codex"),
        crate::provider::Provider::Codex,
        "a session pinned to a deleted row keeps its provider"
    );
    std::fs::remove_file(&path).ok();
}

/// A row deleted from the config keeps its slot, parked and unlisted, and only `disabled`
/// keeps it out of rotation: `eligible` never reads `removed`. So no key may reach it. An
/// enable would put an account the operator deleted back into rotation on its in-memory
/// credentials until a restart, and a bench would report a save for a row nobody can see.
#[test]
fn a_deleted_row_is_never_benched_or_enabled() {
    let bob = account("bob@example.com", 0);
    let (manager, path) = build_manager_with_disk(
        config_with(vec![account("alice@example.com", 0), bob.clone()]),
        "deleted-row-keys",
    );
    manager.snapshot(OffsetDateTime::now_utc());
    let without_alice = delete_from_disk(&manager, &path, vec![bob]);
    assert_eq!(names(&without_alice), ["bob@example.com"]);
    let on_disk = std::fs::read(&path).expect("read the test config");

    assert_eq!(
        manager.set_disabled(0, false),
        DisablePersist::NoSuchAccount,
        "an enable must not reach a deleted row"
    );
    {
        let accounts = manager.accounts.read().expect("accounts lock poisoned");
        assert!(accounts[0].removed, "the deleted row is still marked");
        assert!(
            accounts[0].disabled,
            "and still parked: an enable of it would put it back in rotation"
        );
    }
    assert_eq!(
        manager.set_disabled(0, true),
        DisablePersist::NoSuchAccount,
        "a bench of a deleted row is refused too"
    );
    assert_eq!(
        std::fs::read(&path).expect("read the test config"),
        on_disk,
        "nothing was written for a row the file no longer holds"
    );
    std::fs::remove_file(&path).ok();
}

/// The duplicate notice a Codex row gets names the Codex remedy: remove the duplicate or
/// re-import it under its own name. Giving each entry its own `orgUuid`, the Claude remedy,
/// does nothing for two entries that share one ChatGPT login. Every other outcome, and every
/// Claude row, reads exactly as `warning` does.
#[test]
fn the_codex_duplicate_notice_names_the_codex_remedy() {
    const CODEX_DUPLICATE: &str = "NOT SAVED: two config entries share this Codex login \
        — remove the duplicate (tcr remove <name>), or re-import it with --name";
    let outcomes = [
        DisablePersist::Persisted,
        DisablePersist::NoConfigFile,
        DisablePersist::NoSuchAccount,
        DisablePersist::NoEntry,
        DisablePersist::Ambiguous,
        DisablePersist::WriteFailed,
    ];
    for disabled in [true, false] {
        assert_eq!(
            DisablePersist::Ambiguous.warning_for(disabled, crate::provider::Provider::Codex),
            Some(CODEX_DUPLICATE)
        );
        for outcome in outcomes {
            assert_eq!(
                outcome.warning_for(disabled, crate::provider::Provider::Anthropic),
                outcome.warning(disabled),
                "a Claude row reads {outcome:?} as it always has"
            );
            if outcome != DisablePersist::Ambiguous {
                assert_eq!(
                    outcome.warning_for(disabled, crate::provider::Provider::Codex),
                    outcome.warning(disabled),
                    "{outcome:?} says the same about either provider"
                );
            }
        }
    }
    assert!(
        DisablePersist::Ambiguous
            .warning_for(true, crate::provider::Provider::Anthropic)
            .is_some_and(|text| text.ends_with("give each its own orgUuid")),
        "the Claude duplicate notice keeps its remedy"
    );
}

/// A manager with no account, for the Codex traffic counts, which no account owns.
fn traffic_manager() -> Arc<Manager> {
    manager_of(Vec::new())
}

/// The refused count covers the last ten minutes, by the minute: a 429 drops out once ten
/// whole minutes have passed since the minute it was sent in. The latest refusal's type and
/// time ride with the count, and go when it reaches zero, so the wire never pairs a zero
/// count with a stale type.
///
/// Watched red: summing all ten buckets whatever their minute keeps the first refusal in
/// the count at the tenth minute.
#[test]
fn the_refused_ring_counts_ten_minutes_of_codex_429s() {
    // 2026-01-01T00:00:00Z, on a minute boundary.
    let t0: i64 = 1_767_225_600_000;
    let manager = traffic_manager();
    manager.note_codex_refusal(tcr_status_wire::CodexRefusalKind::UsageLimitReached, t0);
    manager.note_codex_refusal(
        tcr_status_wire::CodexRefusalKind::PoolUnavailable,
        t0 + 300_000,
    );
    assert_eq!(
        manager.codex_traffic(t0 + 599_000),
        tcr_status_wire::CodexTraffic {
            refused_10m: 2,
            last_refusal: Some(tcr_status_wire::CodexRefusalKind::PoolUnavailable),
            last_refusal_at_ms: Some(t0 + 300_000),
            unpooled_10m: 0,
        }
    );
    assert_eq!(manager.codex_traffic(t0 + 600_000).refused_10m, 1);
    let emptied = manager.codex_traffic(t0 + 900_000);
    assert_eq!(emptied.refused_10m, 0);
    assert_eq!(emptied.last_refusal, None);
    assert_eq!(emptied.last_refusal_at_ms, None);
}

/// The unpooled count covers the same ten minutes, by the same minute buckets.
#[test]
fn the_unpooled_ring_counts_routed_relays() {
    let t0: i64 = 1_767_225_600_000;
    let manager = traffic_manager();
    for _ in 0..3 {
        manager.note_codex_unpooled(t0);
    }
    assert_eq!(manager.codex_traffic(t0 + 599_000).unpooled_10m, 3);
    assert_eq!(manager.codex_traffic(t0 + 600_000).unpooled_10m, 0);
    assert_eq!(manager.codex_traffic(t0 + 599_000).refused_10m, 0);
}
