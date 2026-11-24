//! Selection filters by provider first, through every door.

use super::select_test_support::{
    account, config_with, live_manager, live_manager_on_file, pin, pin_of,
};
use super::*;
use crate::config::Account;
use crate::provider::Provider;
use crate::quota::QuotaWindow;
use std::collections::HashSet;
use time::Duration;

const ANTHROPIC_PATH: &str = "/v1/messages";
const CODEX_PATH: &str = "/backend-api/codex/responses";

/// The same account, on the Codex upstream.
fn codex(name: &str) -> Account {
    Account {
        provider: Provider::Codex,
        ..account(name)
    }
}

/// [`account`] as a runtime row, built by the constructor `assemble` uses,
/// for the pure [`Manager::eligible`] predicate.
fn runtime(name: &str) -> AccountRuntime {
    AccountRuntime::from_config(&account(name), false)
}

#[test]
fn a_loaded_strict_codex_peer_exit_is_skipped_by_every_selection_path() {
    let peer = tcr_peer_wire::PeerId([0x55; 32]);
    let mut failures = Vec::new();
    for door in ["pool", "affinity", "locked", "revalidation", "bound"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(crate::paths::CONFIG_FILE);
        let mut config = config_with(vec![
            Account {
                egress: config::Egress::Via(peer),
                egress_strict: true,
                ..codex("alice@example.com/codex")
            },
            codex("bob@example.com/codex"),
        ]);
        config.proxy.port = 9;
        if door == "locked" {
            config.lock_account = Some("alice@example.com/codex".into());
        }
        config::save(&path, &config).unwrap();
        let config = config::load(&path).unwrap();
        assert_eq!(
            config.accounts.len(),
            2,
            "the row remains available for repair"
        );
        assert!(config.accounts[0].egress_strict);
        let manager = live_manager(config);
        let now = OffsetDateTime::now_utc();
        pin(&manager, 77, 0, odt_to_ms(now));
        let got = match door {
            "pool" | "locked" => manager.select(&HashSet::new(), now, None, None, CODEX_PATH, None),
            "affinity" => manager.select(&HashSet::new(), now, None, Some(77), CODEX_PATH, None),
            "revalidation" => {
                manager.select_revalidation(&HashSet::new(), now, None, Some(77), CODEX_PATH)
            }
            "bound" => manager.select_bound(
                0,
                &HashSet::new(),
                now,
                false,
                None,
                None,
                WarmWindow::DEFAULT,
            ),
            _ => unreachable!(),
        };
        let expected = if matches!(door, "locked" | "bound") {
            None
        } else {
            Some(1)
        };
        if got != expected {
            failures.push(format!("{door}: selected {got:?}, expected {expected:?}"));
        }
        if matches!(door, "affinity" | "revalidation") && pin_of(&manager, 77) != Some(1) {
            failures.push(format!(
                "{door}: retained the unusable account's affinity pin"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn supported_peer_exit_settings_keep_their_selection_behavior() {
    let peer = tcr_peer_wire::PeerId([0x55; 32]);
    for (provider, egress, strict) in [
        (Provider::Anthropic, config::Egress::Via(peer), true),
        (Provider::Codex, config::Egress::Via(peer), false),
        (Provider::Codex, config::Egress::Local, true),
    ] {
        let manager = live_manager(config_with(vec![Account {
            provider,
            egress,
            egress_strict: strict,
            ..account("alice@example.com")
        }]));
        let path = match provider {
            Provider::Anthropic => ANTHROPIC_PATH,
            Provider::Codex => CODEX_PATH,
        };
        assert_eq!(
            manager.select(
                &HashSet::new(),
                OffsetDateTime::now_utc(),
                None,
                None,
                path,
                None
            ),
            Some(0),
            "{provider:?} {egress:?}, strict={strict} remains selectable"
        );
    }
}

/// Set account `idx`'s window to `utilization`, resetting two hours out.
fn set_window(
    manager: &Manager,
    idx: usize,
    window: fn(&mut Quota) -> &mut Option<QuotaWindow>,
    utilization: f64,
) {
    let now = OffsetDateTime::now_utc();
    let mut accounts = manager
        .accounts
        .write()
        .unwrap_or_else(PoisonError::into_inner);
    *window(&mut accounts[idx].quota) = Some(QuotaWindow {
        utilization,
        reset: Some(now + Duration::hours(2)),
    });
}

#[test]
fn classify_request_treats_the_codex_responses_path_as_inference() {
    assert_eq!(
        classify_request("/backend-api/codex/responses"),
        RequestClass::Inference
    );
    assert_eq!(classify_request("/v1/messages"), RequestClass::Inference);
    // Every other Codex path is relayed by the proxy before classification matters; it must at
    // least not be mistaken for inference.
    assert_ne!(
        classify_request("/backend-api/wham/usage"),
        RequestClass::Inference
    );
}

/// The pool pick's door: `eligible` refuses a provider mismatch before any
/// other check, in both directions.
///
/// Watched red: delete `eligible`'s `if account.provider != provider`
/// guard and the first assertion passes a Codex account for an Anthropic
/// request.
#[test]
fn eligible_refuses_an_account_of_the_other_provider() {
    let mut codex = runtime("alice@example.com");
    codex.provider = Provider::Codex;
    let anthropic = runtime("bob@example.com");
    let now = OffsetDateTime::now_utc();
    let now_ms = odt_to_ms(now);
    let pacing = PacingConfig::default();
    let empty = HashSet::new();
    // Same call shape as the existing `eligible` tests in this file; only `provider` is new.
    assert!(
        !Manager::eligible(
            &codex,
            0.9,
            None,
            &pacing,
            false,
            now,
            now_ms,
            false,
            Provider::Anthropic,
            None,
            None,
            &empty,
            &empty
        ),
        "a Codex account must never serve an Anthropic request"
    );
    assert!(
        !Manager::eligible(
            &anthropic,
            0.9,
            None,
            &pacing,
            false,
            now,
            now_ms,
            false,
            Provider::Codex,
            None,
            None,
            &empty,
            &empty
        ),
        "an Anthropic account must never serve a Codex request"
    );
    assert!(Manager::eligible(
        &codex,
        0.9,
        None,
        &pacing,
        false,
        now,
        now_ms,
        false,
        Provider::Codex,
        None,
        None,
        &empty,
        &empty
    ));
}

/// One Codex account free and the only Anthropic account disabled: the
/// Anthropic request gets `None`, which the proxy turns into an honest
/// 429, and the Codex request is served.
///
/// Watched red: the same `eligible` guard; without it the Anthropic request
/// lands on the Codex account.
#[test]
fn select_with_only_codex_accounts_free_refuses_an_anthropic_request() {
    let disabled_anthropic = Account {
        disabled: Some(true),
        ..account("bob@example.com")
    };
    let manager = live_manager(config_with(vec![
        codex("alice@example.com"),
        disabled_anthropic,
    ]));
    let picked = manager.select(
        &HashSet::new(),
        OffsetDateTime::now_utc(),
        Some("claude-opus-5"),
        None,
        "/v1/messages",
        None,
    );
    assert_eq!(
        picked, None,
        "an exhausted Anthropic fleet must 429, never spill to Codex"
    );
    let picked = manager.select(
        &HashSet::new(),
        OffsetDateTime::now_utc(),
        Some("gpt-6-astra"),
        None,
        "/backend-api/codex/responses",
        None,
    );
    assert_eq!(
        picked,
        Some(0),
        "the Codex account serves the Codex request"
    );
}

/// A hard lock constrains its own provider only. The other provider still
/// selects from its own pool and never receives the locked credential.
#[test]
fn a_hard_account_lock_never_serves_the_other_provider() {
    let mut config = config_with(vec![codex("alice@example.com"), account("bob@example.com")]);
    config.lock_account = Some("alice@example.com".to_string());
    let manager = live_manager(config);
    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, ANTHROPIC_PATH, None),
        Some(1),
        "a Codex lock must leave the Anthropic pool available"
    );
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, CODEX_PATH, None),
        Some(0),
        "the lock still holds for its own provider's requests"
    );
}

/// A Claude lock leaves Codex selection independent, while its own
/// provider continues to use the exact locked slot.
#[test]
fn a_hard_account_lock_on_an_anthropic_account_never_serves_a_codex_request() {
    let mut config = config_with(vec![account("alice@example.com"), codex("bob@example.com")]);
    config.lock_account = Some("alice@example.com".to_string());
    let manager = live_manager(config);
    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, CODEX_PATH, None),
        Some(1),
        "a Claude lock must leave the Codex pool available"
    );
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, ANTHROPIC_PATH, None),
        Some(0),
        "the lock still holds for its own provider's requests"
    );
}

#[test]
fn provider_isolation_claude_lock_does_not_hide_codex_capacity() {
    let mut config = config_with(vec![account("alice@example.com"), codex("bob@example.com")]);
    config.lock_account = Some("alice@example.com".into());
    let manager = live_manager(config);
    assert!(manager.codex_account_can_serve_eventually(None));
    assert!(manager.codex_account_can_serve_eventually(Some(1)));
    assert!(!manager.codex_account_can_serve_eventually(Some(0)));
    assert_eq!(
        manager.select(
            &HashSet::from([0]),
            OffsetDateTime::now_utc(),
            None,
            None,
            ANTHROPIC_PATH,
            None
        ),
        None,
        "the lock still prevents failover within its own provider"
    );
}

#[test]
fn provider_isolation_codex_lock_uses_its_slot_not_a_shared_name() {
    let mut config = config_with(vec![
        Account {
            disabled: Some(true),
            account_uuid: Some("11111111-1111-1111-1111-111111111111".into()),
            ..codex("alice@example.com")
        },
        Account {
            account_uuid: Some("22222222-2222-2222-2222-222222222222".into()),
            ..codex("alice@example.com")
        },
    ]);
    config.lock_account = Some("alice@example.com".into());
    let manager = live_manager(config);
    assert!(
        !manager.codex_account_can_serve_eventually(None),
        "a healthy same-named row is not the locked slot"
    );
    assert!(!manager.codex_account_can_serve_eventually(Some(1)));
    assert_eq!(
        manager.select(
            &HashSet::from([0]),
            OffsetDateTime::now_utc(),
            None,
            None,
            CODEX_PATH,
            None
        ),
        None,
        "a failed locked slot never falls through to its same-named sibling"
    );
}

#[test]
fn provider_isolation_unsupported_exit_excludes_only_its_exact_slot() {
    let manager = live_manager(config_with(vec![
        Account {
            account_uuid: Some("11111111-1111-1111-1111-111111111111".into()),
            egress: config::Egress::Via(tcr_peer_wire::PeerId([0x55; 32])),
            egress_strict: true,
            ..codex("alice@example.com")
        },
        Account {
            account_uuid: Some("22222222-2222-2222-2222-222222222222".into()),
            ..codex("alice@example.com")
        },
    ]));
    let now = OffsetDateTime::now_utc();
    pin(&manager, 77, 1, odt_to_ms(now));
    assert_eq!(
        manager.codex_accounts_with_unsupported_exit(),
        HashSet::from([0])
    );
    assert!(manager.codex_account_can_serve_eventually(Some(1)));
    assert_eq!(
        manager.select(&HashSet::new(), now, None, Some(77), CODEX_PATH, None),
        Some(1)
    );
    assert_eq!(
        pin_of(&manager, 77),
        Some(1),
        "the healthy same-named account keeps its pin"
    );
}

#[test]
fn provider_isolation_request_reads_do_not_wait_for_a_config_fsync() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(crate::paths::CONFIG_FILE);
    let mut config = config_with(vec![account("alice@example.com"), codex("bob@example.com")]);
    config.proxy.port = 9;
    config::save(&path, &config).unwrap();
    let manager = live_manager_on_file(config.clone(), path.clone());
    assert_eq!(manager.provider_account_count(Provider::Anthropic), 1);
    let writing = manager.config_write.lock().unwrap();
    config.accounts.remove(0);
    config::save(&path, &config).unwrap();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let worker_manager = Arc::clone(&manager);
    let worker = std::thread::spawn(move || {
        started_tx.send(()).unwrap();
        let count = worker_manager.provider_account_count(Provider::Anthropic);
        let selected = worker_manager.select(
            &HashSet::new(),
            OffsetDateTime::now_utc(),
            None,
            None,
            ANTHROPIC_PATH,
            None,
        );
        let codex_available = worker_manager.codex_account_can_serve_eventually(None);
        done_tx.send((count, selected, codex_available)).unwrap();
    });
    started_rx.recv().unwrap();
    let result = done_rx.recv_timeout(std::time::Duration::from_millis(500));
    drop(writing);
    worker.join().unwrap();
    assert_eq!(
        result.expect("request reads must use cached state during an in-flight config write"),
        (1, Some(0), true)
    );
    assert_eq!(
        manager.provider_account_count(Provider::Anthropic),
        0,
        "the completed delete is reloaded on the next request"
    );
    assert_eq!(
        manager.select(
            &HashSet::new(),
            OffsetDateTime::now_utc(),
            None,
            None,
            ANTHROPIC_PATH,
            None
        ),
        None
    );
}

/// Send once on account `first_idx` (0 is Anthropic, 1 is Codex), then expect the
/// other provider's account to send within 250 ms, failing with `why` if it is paced.
/// Answers the other account's index and the path each index sends on.
async fn send_on_one_provider_then_the_other(
    manager: &Manager,
    first_idx: usize,
    why: &str,
) -> (usize, [&'static str; 2]) {
    let second_idx = 1 - first_idx;
    let paths = [ANTHROPIC_PATH, CODEX_PATH];
    manager.throttle_send(paths[first_idx], first_idx).await;
    tokio::time::timeout(
        std::time::Duration::from_millis(250),
        manager.throttle_send(paths[second_idx], second_idx),
    )
    .await
    .expect(why);
    (second_idx, paths)
}

#[tokio::test]
async fn provider_isolation_fleet_throttle_does_not_pace_the_other_upstream() {
    for first in [Provider::Anthropic, Provider::Codex] {
        let mut config = config_with(vec![account("alice@example.com"), codex("bob@example.com")]);
        config.fleet_throttle = ThrottleConfig {
            min_spacing_ms: Some(60_000),
            burst: Some(1),
        };
        let manager = live_manager(config);
        let first_idx = match first {
            Provider::Anthropic => 0,
            Provider::Codex => 1,
        };
        let (second_idx, paths) = send_on_one_provider_then_the_other(
            &manager,
            first_idx,
            "the other provider has its own fleet burst",
        )
        .await;
        for idx in [first_idx, second_idx] {
            assert!(
                tokio::time::timeout(
                    std::time::Duration::from_millis(10),
                    manager.throttle_send(paths[idx], idx)
                )
                .await
                .is_err(),
                "same-provider requests still consume the fleet throttle"
            );
        }
    }
}

async fn assert_org_throttle_is_provider_scoped(org_uuid: Option<&str>) {
    for first_idx in [0, 1] {
        let mut accounts = vec![
            account("alice@example.com"),
            codex("bob@example.com"),
            account("carol@example.com"),
            codex("dave@example.com"),
        ];
        for account in &mut accounts {
            account.org_uuid = org_uuid.map(str::to_string);
            account.org_name = Some("Example Org".into());
        }
        let mut config = config_with(accounts);
        config.account_throttle = ThrottleConfig {
            min_spacing_ms: Some(60_000),
            burst: Some(1),
        };
        let manager = live_manager(config);
        let (second_idx, paths) = send_on_one_provider_then_the_other(
            &manager,
            first_idx,
            "equal organization identities on different providers have independent bursts",
        )
        .await;
        for idx in [first_idx, second_idx] {
            assert!(
                tokio::time::timeout(
                    std::time::Duration::from_millis(10),
                    manager.throttle_send(paths[idx], idx + 2)
                )
                .await
                .is_err(),
                "accounts in one organization on the same provider still share its throttle"
            );
        }
    }
}

#[tokio::test]
async fn provider_isolation_org_throttle_uuid_does_not_pace_the_other_upstream() {
    assert_org_throttle_is_provider_scoped(Some("11111111-1111-1111-1111-111111111111")).await;
}

#[tokio::test]
async fn provider_isolation_org_throttle_name_does_not_pace_the_other_upstream() {
    assert_org_throttle_is_provider_scoped(None).await;
}

/// **A SESSION'S PIN NEVER CROSSES PROVIDERS.** The affinity fast path
/// serves a pin that fails `eligible` whenever the account is still alive
/// (`Manager::account_hard_ok`) and the session is warm: the over-threshold
/// pin-honor serve. A pin on the other provider's account passes
/// `account_hard_ok`, which knows nothing about the request, so without its
/// own check that serve hands an Anthropic request to a Codex account.
///
/// A DIVERT and not a re-key: the mismatch is a fact about this request,
/// like the model-class and strict-group diverts, so the pin stays where
/// the session's other traffic is warm.
///
/// Watched red: delete the fast path's provider divert and the request is
/// served on the pinned Codex account (`Some(0)`) by the pin-honor serve.
#[test]
fn a_pin_on_the_other_providers_account_is_diverted_never_served() {
    let manager = live_manager(config_with(vec![
        codex("alice@example.com"),
        account("bob@example.com"),
    ]));
    let now = OffsetDateTime::now_utc();
    // Touched just now, so the session is warm and the pin-honor serve is live.
    pin(&manager, 7, 0, odt_to_ms(now));
    assert_eq!(
        manager.select(&HashSet::new(), now, None, Some(7), ANTHROPIC_PATH, None),
        Some(1),
        "the Anthropic request is served by the Anthropic account"
    );
    assert_eq!(
        pin_of(&manager, 7),
        Some(0),
        "a divert, not a re-key: the session keeps its pin"
    );
}

/// **THE CONTROL-PREFERRED OVERLAY'S DOOR.** Only the Codex responses path
/// is inference; every other `/backend-api/` path classifies as
/// control-preferred, and the overlay routes it through
/// `Manager::control_eligible`, which deliberately bypasses `eligible`. The
/// control account is an Anthropic identity (`assemble` refuses a Codex
/// one), so without a provider check a Codex control-plane request is
/// routed onto it. The proxy relays those paths under the client's own
/// bearer before selection; this is the picker's own guarantee.
///
/// Watched red: drop the overlay's provider check and the Codex request is
/// served on the Anthropic control account (`Some(1)`).
#[test]
fn the_control_overlay_never_routes_the_other_providers_request() {
    let mut config = config_with(vec![codex("alice@example.com"), account("bob@example.com")]);
    config.control_account = Some(config::ControlAccountRef::Name(
        "bob@example.com".to_string(),
    ));
    let manager = live_manager(config);
    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager.select(
            &HashSet::new(),
            now,
            None,
            None,
            "/backend-api/wham/usage",
            None
        ),
        Some(0),
        "a Codex control-plane request degrades to its own provider's pool, never the Anthropic control account"
    );
    // The control: an Anthropic control-plane request still prefers it.
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, "/v1/organizations", None),
        Some(1),
        "control-preferred routing is unchanged for its own provider"
    );
}

/// **THE FABLE LAST RESORT'S DOOR.** `pick_fable_last_resort` mirrors
/// `eligible`'s gates by hand rather than calling it, and a Codex account
/// carries no `7d_oi` window at all (tcr reads no model-scoped window from
/// Codex), so it reads as the FRESHEST candidate and would win the last
/// resort outright.
///
/// Watched red: drop the last resort's provider check and the Fable request
/// is served on the Codex account (`Some(1)`).
#[test]
fn the_fable_last_resort_never_reaches_the_other_provider() {
    let manager = live_manager(config_with(vec![
        account("alice@example.com"),
        codex("bob@example.com"),
    ]));
    // Over the predicted Fable threshold (0.90), not rejected upstream: the
    // shape that sends a Fable request to the last resort.
    set_window(&manager, 0, |q| &mut q.seven_day_oi, 0.97);
    assert_eq!(
        manager.select(
            &HashSet::new(),
            OffsetDateTime::now_utc(),
            Some("claude-fable-5"),
            None,
            ANTHROPIC_PATH,
            None
        ),
        Some(0),
        "the last resort spends its one request on the Anthropic account, never the Codex one"
    );
}

/// **THE REVALIDATION SERVE'S DOOR**, the one the proxy opens when the pick
/// comes back empty (`revalidationServe` ships ON). Its fallback serves the
/// least-utilized account that passes the hard gates alone, and an idle
/// Codex account is the least utilized account on any mixed fleet whose
/// Anthropic half is over the soft threshold.
///
/// Watched red: drop the provider check from `select_revalidation`'s
/// `hard_ok` and the Anthropic request is revalidated onto the Codex
/// account (`Some(1)`).
#[test]
fn codex_and_claude_revalidation_valves_are_independent() {
    for order in [
        [(CODEX_PATH, 1), (ANTHROPIC_PATH, 0)],
        [(ANTHROPIC_PATH, 0), (CODEX_PATH, 1)],
    ] {
        let manager = live_manager(config_with(vec![
            account("alice@example.com"),
            codex("bob@example.com"),
        ]));
        set_window(&manager, 0, |quota| &mut quota.five_hour, 0.99);
        set_window(&manager, 1, |quota| &mut quota.five_hour, 0.99);
        let now = OffsetDateTime::now_utc();
        for (path, index) in order {
            assert_eq!(
                manager.select(&HashSet::new(), now, None, None, path, None),
                None
            );
            assert_eq!(
                manager.select_revalidation(&HashSet::new(), now, None, None, path),
                Some(index),
                "one provider must not spend the other's revalidation valve"
            );
            assert_eq!(
                manager.select_revalidation(&HashSet::new(), now, None, None, path),
                None,
                "same-provider bursts remain bounded"
            );
        }
    }
}

#[test]
fn revalidation_never_serves_the_other_provider() {
    let manager = live_manager(config_with(vec![
        account("alice@example.com"),
        codex("bob@example.com"),
    ]));
    set_window(&manager, 0, |q| &mut q.five_hour, 0.95);
    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, ANTHROPIC_PATH, None),
        None,
        "precondition: the Anthropic pool reads exhausted to the normal pick"
    );
    assert_eq!(
        manager.select_revalidation(&HashSet::new(), now, None, None, ANTHROPIC_PATH),
        Some(0),
        "revalidation serves the over-threshold Anthropic account, never the idle Codex one"
    );
}

/// **THE REVALIDATION SERVE'S PIN-HONOR DOOR**, the pin half of the test
/// above. A session pinned on the Codex account sends an Anthropic request
/// into `select_revalidation`: the pin-honor serve must refuse the pin (its
/// `hard_ok` asks the provider first) and the fallback serves the Anthropic
/// account. The pin itself is KEPT: the Codex account is alive
/// (`Manager::account_hard_ok` judges the account, not the request), so this
/// is a divert like `select`'s other-provider divert, never a re-key.
///
/// Watched red twice: drop the provider check from `select_revalidation`'s
/// `hard_ok` and the pin-honor serve hands the request to the Codex account
/// (`Some(0)`); make the keep-pin test ask the provider too (a re-key) and
/// the session's pin moves to the Anthropic account.
#[test]
fn revalidation_diverts_a_pin_on_the_other_providers_account_and_keeps_it() {
    let manager = live_manager(config_with(vec![
        codex("alice@example.com"),
        account("bob@example.com"),
    ]));
    let now = OffsetDateTime::now_utc();
    // Touched just now: a warm session, so nothing but the provider stands
    // between the pin and the pin-honor serve.
    pin(&manager, 7, 0, odt_to_ms(now));
    assert_eq!(
        manager.select_revalidation(&HashSet::new(), now, None, Some(7), ANTHROPIC_PATH),
        Some(1),
        "the Anthropic request is revalidated onto the Anthropic account"
    );
    assert_eq!(
        pin_of(&manager, 7),
        Some(0),
        "a divert, not a re-key: the session keeps its pin"
    );
}

/// The Fable weekly bucket is an Anthropic fact; a Codex request never
/// consults it, at either place a request's class is computed
/// (`select_with_group` and `select_revalidation`).
///
/// The model id is a fixture that merely CONTAINS "fable", because that is
/// all `crate::model::is_fable_model` asks. No Codex account carries a
/// `7d_oi` window (tcr reads no model-scoped window from Codex), so one is
/// set by hand here: it is the only way to observe the gate, and the test
/// pins the rule rather than a failure seen live.
///
/// Watched red: drop the `provider == Anthropic` term from either `is_fable`
/// and that call answers `None` (the window reads as a hard rejection).
#[test]
fn a_codex_request_never_consults_the_fable_weekly_bucket() {
    let now = OffsetDateTime::now_utc();
    let manager = live_manager(config_with(vec![codex("alice@example.com")]));
    set_window(&manager, 0, |q| &mut q.seven_day_oi, 1.0);
    assert_eq!(
        manager.select(
            &HashSet::new(),
            now,
            Some("gpt-6-fable"),
            None,
            CODEX_PATH,
            None
        ),
        Some(0),
        "select must not gate a Codex request on the Fable bucket"
    );
    // A second manager: the revalidation serve spends a fleet-wide 2 s
    // window per serve, and this assertion must not depend on that.
    let manager = live_manager(config_with(vec![codex("alice@example.com")]));
    set_window(&manager, 0, |q| &mut q.seven_day_oi, 1.0);
    assert_eq!(
        manager.select_revalidation(&HashSet::new(), now, Some("gpt-6-fable"), None, CODEX_PATH),
        Some(0),
        "select_revalidation must not gate a Codex request on the Fable bucket"
    );
}
