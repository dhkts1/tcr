//! Tests for the per-account verbs in `cli/accounts.rs`: remove, priority, enable and
//! disable, request-key, the control account and egress.

use super::*;
/// `tcr request-key` prints the key the file holds, and with none it fails with a line
/// that says the server writes one, and writes nothing itself.
#[test]
fn request_key_prints_the_saved_key_and_never_mints_one() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let keyed = dir.path().join("keyed.json");
    fs::write(
            &keyed,
            r#"{"proxy": {"requestKey": "4444444444444444444444444444444444444444444444444444444444444444"}, "accounts": []}"#,
        )
        .expect("writes");
    assert_eq!(
        request_key(&keyed).expect("a saved key prints"),
        "4444444444444444444444444444444444444444444444444444444444444444"
    );

    for body in [r#"{"accounts": []}"#, r#"{"proxy": {"requestKey": ""}}"#] {
        let bare = dir.path().join("bare.json");
        fs::write(&bare, body).expect("writes");
        let err = request_key(&bare).expect_err("no key is an error");
        assert!(
            err.to_string().contains("writes one there when it starts"),
            "{err}"
        );
        assert_eq!(
            fs::read_to_string(&bare).expect("reads"),
            body,
            "the CLI never writes a key"
        );
    }
}

#[tokio::test]
async fn remove_deletes_named_account_leaving_siblings() {
    let path = config_on_a_dead_port("remove").await;
    remove_account(&path, "alice@example.com").await.unwrap();
    let config = load(&path);
    assert_eq!(config.accounts.len(), 1);
    assert_eq!(config.accounts[0].name, "bob@example.com");
    fs::remove_file(&path).ok();
}

#[tokio::test]
async fn remove_preserves_unmodelled_extra_fields() {
    let path = config_on_a_dead_port("remove-extra").await;
    remove_account(&path, "alice@example.com").await.unwrap();
    let raw = fs::read_to_string(&path).unwrap();
    let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
    // The #1 silent-loss risk: unmodelled top-level keys survive the edit.
    assert_eq!(value["quotaProbeSeconds"], serde_json::json!(120));
    assert!(value["routes"].is_array());
    fs::remove_file(&path).ok();
}

#[tokio::test]
async fn remove_nonexistent_errors_and_leaves_file_byte_identical() {
    let path = config_on_a_dead_port("remove-miss").await;
    let before = fs::read_to_string(&path).unwrap();
    let result = remove_account(&path, "nobody@example.com").await;
    assert!(result.is_err());
    let after = fs::read_to_string(&path).unwrap();
    assert_eq!(before, after, "a failed resolve must not write the config");
    fs::remove_file(&path).ok();
}

/// THE BITING TEST for remove, end to end through the PRODUCTION path: a
/// real hybrid listener, a real socket, the real router. Pre-change
/// `remove_account` only ever touched the file, so the running manager's
/// rotation kept serving the deleted account's identity until restart —
/// the same defect `set_enabled_parks_the_account_in_the_running_proxy`
/// exists to catch, one level more destructive here because the record is
/// also gone.
#[tokio::test]
async fn remove_parks_the_account_in_the_running_proxy_before_deleting() {
    let (listener, port) = crate::test_support::loopback::listener().await;
    let path = config_on_a_dead_port("live-remove").await;
    let mut config = load(&path);
    config.proxy.port = port;

    let manager =
        crate::test_support::live_proxy::serve(listener, config.clone(), Some(path.clone()));

    point_at_port(&path, port);

    remove_account(&path, "alice@example.com").await.unwrap();

    // 1. THE RUNNING ROTATION: out of it at once, not at next boot. It is
    // no longer listed (the snapshot re-reads the file first), and no pick
    // lands on it even though it has the better priority.
    let live = manager.snapshot(OffsetDateTime::now_utc());
    let listed: Vec<&str> = live.accounts.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(
        listed,
        vec!["bob@example.com"],
        "the serving process dropped it"
    );
    let picked = manager.select(
        &std::collections::HashSet::new(),
        OffsetDateTime::now_utc(),
        None,
        None,
        "/v1/messages",
        None,
    );
    assert_eq!(picked, Some(1), "the deleted account never serves again");

    // 2. …and the file no longer carries the record at all.
    let config = load(&path);
    assert_eq!(config.accounts.len(), 1);
    assert_eq!(config.accounts[0].name, "bob@example.com");
    fs::remove_file(&path).ok();
}

/// **Deleting a Codex account takes out that row and no other**, in the file and in
/// the running proxy, though an Anthropic row shares its identity. The two rows below
/// carry the same `accountUuid` and no org, the shape in which identity alone calls
/// them one account; `config::remove_account_entry` finds the entry it deletes, and
/// the proxy's reload finds the runtime row the file lost, both by identity. Only the
/// provider keeps either from landing on the Anthropic row.
///
/// Watched red: drop the provider guard from `identity::same_identity`, and the
/// delete is refused as ambiguous and changes nothing.
#[tokio::test]
async fn remove_takes_a_codex_account_out_and_leaves_its_anthropic_twin() {
    let (listener, port) = crate::test_support::loopback::listener().await;
    let path = write_config(
        "live-remove-codex",
        &format!(
            r#"{{ "proxy": {{ "port": {port} }}, "accounts": [
                     {{ "name": "alice@example.com", "type": "oauth", "accessToken": "at-a",
                        "refreshToken": "rt-a", "expiresAt": 4102444800000, "priority": 0,
                        "accountUuid": "11111111-1111-4111-8111-111111111111" }},
                     {{ "name": "alice@example.com/codex", "provider": "codex", "type": "oauth",
                        "accessToken": "at-c", "refreshToken": "rt-c", "expiresAt": 4102444800000,
                        "priority": 0, "accountUuid": "11111111-1111-4111-8111-111111111111",
                        "credentialVersion": 2 }}
                   ] }}"#
        ),
    );
    let manager = crate::test_support::live_proxy::serve(listener, load(&path), Some(path.clone()));

    remove_account(&path, "alice@example.com/codex")
        .await
        .expect("the Codex row is deleted");

    let live = manager.snapshot(OffsetDateTime::now_utc());
    let listed: Vec<(&str, Provider)> = live
        .accounts
        .iter()
        .map(|a| (a.name.as_str(), a.provider))
        .collect();
    assert_eq!(
        listed,
        vec![("alice@example.com", Provider::Anthropic)],
        "the serving process dropped the Codex row and only it"
    );
    let pick = |path: &str| {
        manager.select(
            &std::collections::HashSet::new(),
            OffsetDateTime::now_utc(),
            None,
            None,
            path,
            None,
        )
    };
    assert_eq!(
        pick(crate::provider::CODEX_RESPONSES_PATH),
        None,
        "the deleted Codex account never serves again"
    );
    assert_eq!(
        pick("/v1/messages"),
        Some(0),
        "the Anthropic twin still serves"
    );

    let config = load(&path);
    let on_disk: Vec<(&str, Provider, &str)> = config
        .accounts
        .iter()
        .map(|a| (a.name.as_str(), a.provider, a.access_token.as_str()))
        .collect();
    assert_eq!(
        on_disk,
        vec![("alice@example.com", Provider::Anthropic, "at-a")],
        "the file lost the Codex row and kept the Anthropic one, credential and all"
    );
    fs::remove_file(&path).ok();
}

/// The proxy rejected our api-key: the one arm that must NOT write. If
/// this arm wrote instead, the config would say the account is gone while
/// the running proxy — the one we couldn't even authenticate to, let
/// alone confirm parked the account — keeps rotating it, permanently.
#[tokio::test]
async fn remove_refuses_on_unauthorized_without_writing() {
    let (listener, port) = crate::test_support::loopback::listener().await;
    let server_config: Config = serde_json::from_str(&format!(
        r#"{{ "proxy": {{ "port": {port}, "apiKey": "correct-key" }}, "accounts": [] }}"#
    ))
    .unwrap();
    crate::test_support::live_proxy::serve(listener, server_config, None);

    let path = config_on_a_dead_port("remove-unauth").await;
    let raw = fs::read_to_string(&path).unwrap();
    let mut doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
    doc["proxy"]["port"] = serde_json::json!(port);
    doc["proxy"]["apiKey"] = serde_json::json!("wrong-key");
    fs::write(&path, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
    let before = fs::read_to_string(&path).unwrap();

    let err = remove_account(&path, "alice@example.com")
        .await
        .expect_err("a rejected api-key must refuse the removal");
    assert!(
        err.to_string().contains("NOT changed"),
        "the refusal must say nothing changed: {err}"
    );
    assert_eq!(
        before,
        fs::read_to_string(&path).unwrap(),
        "an Unauthorized removal must leave the config byte-identical"
    );
    fs::remove_file(&path).ok();
}

/// An ambiguous query is refused by the SERVER and the CLI does not fall
/// back to the file's own resolution — a removal applied to the wrong row
/// is unrecoverable, unlike a mis-applied disable.
#[tokio::test]
async fn remove_refuses_an_ambiguous_query_without_writing() {
    let (path, listener) = config_on_a_scratch_port("remove-ambiguous").await;

    // The ambiguity is in the LIVE rotation only: the file keeps unique
    // names (the loader guarantees that), and the running fleet is broken by
    // hand afterwards. That is the split this test is about — the CLI must
    // not resolve against the file when the server refused.
    let before = serve_a_fleet_with_two_alices(&path, listener);

    let err = remove_account(&path, "alice@example.com")
        .await
        .expect_err("an ambiguous query must not be applied");
    let text = err.to_string();
    assert!(
        text.contains("ambiguous") && text.matches("alice@example.com").count() >= 2,
        "the refusal names the candidates: {text}"
    );
    assert_eq!(
        before,
        fs::read_to_string(&path).unwrap(),
        "a refused removal leaves the config byte-identical"
    );
    fs::remove_file(&path).ok();
}

// --- priority ----------------------------------------------------------
//
// `config_on_a_dead_port`, never `TWO_ACCOUNTS`' literal 3456: `set_priority`
// dials the configured port for real, and 3456 is the live fleet proxy.

#[tokio::test]
async fn set_priority_explicit_writes_int() {
    let path = config_on_a_dead_port("prio-n").await;
    set_priority(&path, "bob@example.com", PriorityArg::N(7))
        .await
        .unwrap();
    let config = load(&path);
    assert_eq!(config.accounts[1].priority, Some(7));
    fs::remove_file(&path).ok();
}

#[tokio::test]
async fn set_priority_first_is_min0_minus_one() {
    // Existing priorities are 0 and 1 → min(0, {0,1}) - 1 = -1.
    let path = config_on_a_dead_port("prio-first").await;
    set_priority(&path, "bob@example.com", PriorityArg::First)
        .await
        .unwrap();
    let config = load(&path);
    assert_eq!(config.accounts[1].priority, Some(-1));
    fs::remove_file(&path).ok();
}

#[tokio::test]
async fn set_priority_last_is_max0_plus_one() {
    // Existing priorities are 0 and 1 → max(0, {0,1}) + 1 = 2.
    let path = config_on_a_dead_port("prio-last").await;
    set_priority(&path, "alice@example.com", PriorityArg::Last)
        .await
        .unwrap();
    let config = load(&path);
    assert_eq!(config.accounts[0].priority, Some(2));
    fs::remove_file(&path).ok();
}

#[tokio::test]
async fn set_priority_nonexistent_errors_and_leaves_file_byte_identical() {
    let path = config_on_a_dead_port("prio-miss").await;
    let before = fs::read_to_string(&path).unwrap();
    let result = set_priority(&path, "nobody@example.com", PriorityArg::N(3)).await;
    assert!(result.is_err());
    let after = fs::read_to_string(&path).unwrap();
    assert_eq!(before, after);
    fs::remove_file(&path).ok();
}

/// `tcr priority bob --first` against a REAL proxy on an ephemeral port: the
/// serving process routes the next fresh session to bob, and the file
/// carries the new value too. Before the live route, only the file moved and
/// the running rotation kept picking alice until a restart.
#[tokio::test]
async fn set_priority_reorders_the_running_proxy_and_the_file() {
    let (path, listener) = config_on_a_scratch_port("live-priority").await;

    let manager = crate::test_support::live_proxy::serve(listener, load(&path), Some(path.clone()));

    let fresh_pick = |m: &Manager| {
        m.select(
            &std::collections::HashSet::new(),
            OffsetDateTime::now_utc(),
            None,
            None,
            "/v1/messages",
            None,
        )
    };
    assert_eq!(fresh_pick(&manager), Some(0), "alice leads at boot");

    set_priority(&path, "bob@example.com", PriorityArg::First)
        .await
        .unwrap();

    assert_eq!(
        fresh_pick(&manager),
        Some(1),
        "the SERVING process now routes a fresh session to bob"
    );
    assert_eq!(
        load(&path).accounts[1].priority,
        Some(-1),
        "and the file agrees"
    );
    fs::remove_file(&path).ok();
}

/// A refused query (the live rotation holds two rows of one name) changes
/// neither the file nor the running proxy's order.
#[tokio::test]
async fn set_priority_refuses_an_ambiguous_live_query_without_writing() {
    let (path, listener) = config_on_a_scratch_port("live-priority-ambiguous").await;

    let before = serve_a_fleet_with_two_alices(&path, listener);

    let err = set_priority(&path, "alice@example.com", PriorityArg::First)
        .await
        .expect_err("an ambiguous live query must not be applied");
    assert!(err.to_string().contains("ambiguous"), "{err}");
    assert_eq!(before, fs::read_to_string(&path).unwrap());
    fs::remove_file(&path).ok();
}

// --- config set / config show running -----------------------------------
//
// Every test dials a port it bound itself (a scratch proxy) or one nothing
// listens on (`config_on_a_dead_port`): never 3456, the live proxy.

/// Two Claude and two Codex accounts with these priorities, parsed in memory:
/// no file, and nothing to dial.
fn priority_fleet(alice: i64, carol: i64, bob: i64, dana: i64) -> Config {
    serde_json::from_value(serde_json::json!({
        "accounts": [
            { "name": "alice@example.com", "accessToken": "at-a", "priority": alice },
            { "name": "carol@example.com", "accessToken": "at-c", "priority": carol },
            { "name": "bob@example.com/codex", "provider": "codex", "accessToken": "at-b",
              "priority": bob },
            { "name": "dana@example.com/codex", "provider": "codex", "accessToken": "at-d",
              "priority": dana }
        ]
    }))
    .unwrap()
}

/// `--first` and `--last` take the min and max over the row's OWN provider,
/// with the same `0` seed: Claude and Codex accounts rotate apart. A Codex
/// row's relative move names the end of the Codex rotation it now holds; an
/// explicit value, and any Claude move, prints the line it always printed.
#[test]
fn priority_first_and_last_stay_within_the_rows_provider() {
    let fleet = priority_fleet(1, -1, 4, -4);
    assert_eq!(
        priority_value(&fleet, Provider::Anthropic, PriorityArg::First),
        -2,
        "a min over both providers would give -5"
    );
    assert_eq!(
        priority_value(&fleet, Provider::Anthropic, PriorityArg::Last),
        2,
        "a max over both providers would give 5"
    );

    let fleet = priority_fleet(6, -6, 2, -2);
    assert_eq!(
        priority_value(&fleet, Provider::Codex, PriorityArg::First),
        -3
    );
    assert_eq!(
        priority_value(&fleet, Provider::Codex, PriorityArg::Last),
        3
    );

    assert_eq!(
        priority_message(
            "bob@example.com/codex",
            -3,
            Provider::Codex,
            PriorityArg::First
        ),
        "Set priority of 'bob@example.com/codex' to -3, the front of the Codex rotation."
    );
    assert_eq!(
        priority_message(
            "bob@example.com/codex",
            3,
            Provider::Codex,
            PriorityArg::Last
        ),
        "Set priority of 'bob@example.com/codex' to 3, the back of the Codex rotation."
    );
    assert_eq!(
        priority_message(
            "bob@example.com/codex",
            5,
            Provider::Codex,
            PriorityArg::N(5)
        ),
        "Set priority of 'bob@example.com/codex' to 5."
    );
    assert_eq!(
        priority_message(
            "alice@example.com",
            -2,
            Provider::Anthropic,
            PriorityArg::First
        ),
        "Set priority of 'alice@example.com' to -2."
    );
}

/// `tcr priority <codex row> --first` writes the value worked out over the
/// Codex rows: `set_priority` hands `priority_value` the row's provider, which
/// the pure test above cannot see. On a port nothing serves, so the live half
/// finds no proxy and the file is the whole truth.
#[tokio::test]
async fn priority_on_a_codex_row_writes_its_own_providers_value() {
    let port = crate::test_support::loopback::unused_port();
    let json = serde_json::json!({
        "proxy": { "port": port },
        "accounts": [
            { "name": "alice@example.com", "type": "oauth", "accessToken": "at-a",
              "priority": 6 },
            { "name": "carol@example.com", "type": "oauth", "accessToken": "at-c",
              "priority": -6 },
            { "name": "bob@example.com/codex", "provider": "codex", "type": "oauth",
              "accessToken": "at-b", "credentialVersion": 2, "priority": 2 },
            { "name": "dana@example.com/codex", "provider": "codex", "type": "oauth",
              "accessToken": "at-d", "credentialVersion": 2, "priority": -2 }
        ]
    })
    .to_string();
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed["proxy"]["port"], serde_json::json!(port));
    assert_ne!(
        parsed["proxy"]["port"],
        serde_json::json!(3456),
        "never the port a live proxy serves on"
    );
    let path = write_config("priority-codex", &json);

    set_priority(&path, "bob@example.com/codex", PriorityArg::First)
        .await
        .expect("no proxy answers, so the file is written");
    assert_eq!(
        load(&path).accounts[2].priority,
        Some(-3),
        "min(0, the Codex rows' 2 and -2) - 1; over every row it would be -7"
    );
    fs::remove_file(&path).ok();
}

// --- group ---------------------------------------------------------

/// The offline half of `tcr control` (no server, or one too old for the live route)
/// refuses a Codex account the way the live route does, so `tcr control` cannot
/// persist a `controlAccount` the next boot would ignore. The file is left as it was.
///
/// The config names a port nothing listens on (`test_support::loopback::unused_port`): an
/// edit asks whether a
/// server is running on the config's port, and without one that is 3456, where a live
/// tcr may be serving.
///
/// Watched red: drop the provider check from `write_control_account` and the file
/// names the Codex row.
#[tokio::test]
async fn write_control_account_refuses_a_codex_account_and_writes_nothing() {
    let port = crate::test_support::loopback::unused_port();
    let path = write_config(
        "control-codex",
        &serde_json::json!({
            "proxy": { "port": port },
            "accounts": [
                { "name": "alice@example.com", "type": "oauth", "accessToken": "at-a",
                  "expiresAt": 1_893_456_000_000_i64, "priority": 0 },
                { "name": "bob@example.com/codex", "provider": "codex", "type": "oauth",
                  "accountUuid": "11111111-1111-4111-8111-111111111111", "accessToken": "at-b",
                  "refreshToken": "rt-b", "expiresAt": 1_893_456_000_000_i64, "priority": 0 }
            ]
        })
        .to_string(),
    );
    let before = fs::read_to_string(&path).unwrap();
    let err = write_control_account(&path, Some("bob@example.com/codex"))
        .expect_err("a Codex account cannot be the control account");
    let message = format!("{err:#}");
    assert!(
        message.contains("bob@example.com/codex")
            && message.contains(crate::manager::CODEX_HAS_NO_CONTROL_ACCOUNT),
        "{message}"
    );
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        before,
        "nothing was written"
    );
    assert_eq!(
        write_control_account(&path, Some("alice@example.com")).unwrap(),
        Some("alice@example.com".to_string()),
        "control: the Anthropic row is set"
    );
    fs::remove_file(&path).ok();
}

// --- the exit lock on a Codex account -------------------------------------

/// `tcr peer account <codex> --exits-from <peer>` is refused, and so is
/// `--must` on its own, which would make an old pin strict: a Codex request
/// always leaves from this Mac. Each refusal leaves the file byte-identical,
/// and the same pin on a Claude row is written as it always was.
#[tokio::test]
async fn exits_from_a_peer_is_refused_on_a_codex_row() {
    let refusal = "'bob@example.com/codex' is a Codex account, and a Codex request always leaves from this Mac, so it cannot exit from another Mac. Nothing was changed.";
    let path = alice_and_codex_bob("egress-codex", serde_json::json!({})).await;
    let peer = tcr_peer_wire::PeerId([7_u8; 32]);
    let before = fs::read(&path).unwrap();

    let err = set_account_egress(
        &path,
        "bob@example.com/codex",
        Some(config::Egress::Via(peer)),
        None,
    )
    .expect_err("a Codex account cannot be pinned to another Mac");
    assert_eq!(err.to_string(), refusal);
    assert_eq!(fs::read(&path).unwrap(), before, "nothing was written");

    let err = set_account_egress(&path, "bob@example.com/codex", None, Some(true))
        .expect_err("--must alone would make an old pin strict");
    assert_eq!(err.to_string(), refusal);
    assert_eq!(fs::read(&path).unwrap(), before, "nothing was written");

    let (name, pin) = set_account_egress(
        &path,
        "alice@example.com",
        Some(config::Egress::Via(peer)),
        None,
    )
    .expect("control: a Claude row is pinned as before");
    assert_eq!(name, "alice@example.com");
    assert_eq!(pin.egress, config::Egress::Via(peer));
    fs::remove_file(&path).ok();
}

/// `--exits-from local --no-must` stays allowed on a Codex row, so a pin
/// written before the refusal existed can still be cleared.
#[tokio::test]
async fn exits_from_local_still_clears_an_old_codex_pin() {
    let peer = tcr_peer_wire::PeerId([7_u8; 32]);
    let path = alice_and_codex_bob(
        "egress-codex-clear",
        serde_json::json!({ "egress": format!("via {}", peer.to_wire()), "egressStrict": true }),
    )
    .await;
    assert_eq!(
        load(&path).accounts[1].egress_pin(),
        config::EgressPin {
            egress: config::Egress::Via(peer),
            strict: true
        },
        "the fixture carries the old strict pin"
    );

    set_account_egress(
        &path,
        "bob@example.com/codex",
        Some(config::Egress::Local),
        Some(false),
    )
    .expect("local and --no-must clear the old pin");
    assert_eq!(
        load(&path).accounts[1].egress_pin(),
        config::EgressPin {
            egress: config::Egress::Local,
            strict: false
        }
    );
    fs::remove_file(&path).ok();
}

// --- groups and Codex accounts --------------------------------------------

#[tokio::test]
async fn set_enabled_true_writes_disabled_true() {
    let path = config_on_a_dead_port("disable").await;
    set_enabled(&path, "alice@example.com", true).await.unwrap();
    let raw = fs::read_to_string(&path).unwrap();
    let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(value["accounts"][0]["disabled"], serde_json::json!(true));
    fs::remove_file(&path).ok();
}

#[tokio::test]
async fn set_enabled_false_drops_the_disabled_key() {
    let path = config_on_a_dead_port("enable").await;
    // First disable, then re-enable — the key must vanish entirely.
    set_enabled(&path, "alice@example.com", true).await.unwrap();
    set_enabled(&path, "alice@example.com", false)
        .await
        .unwrap();
    let raw = fs::read_to_string(&path).unwrap();
    let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert!(
        value["accounts"][0].get("disabled").is_none(),
        "re-enable must DROP the disabled key, not write false"
    );
    fs::remove_file(&path).ok();
}

/// THE BITING TEST for the CLI half, end to end through the PRODUCTION path:
/// a real hybrid listener (the thing that injects `ClientAddr`), a real socket,
/// the real router, and `set_enabled` as `tcr disable` calls it.
///
/// The two assertions after the call are the whole bug. Pre-change `set_enabled`
/// wrote the file and stopped, so the RUNNING manager's `disabled` stayed
/// `false` and the account it named kept being handed traffic — measured on the
/// live fleet, two accounts deep. A file-only assertion passes against that
/// defect; the in-memory one does not.
#[tokio::test]
async fn set_enabled_parks_the_account_in_the_running_proxy() {
    let (listener, port) = crate::test_support::loopback::listener().await;
    let path = config_on_a_dead_port("live-disable").await;
    let mut config = load(&path);
    config.proxy.port = port;

    // The server owns the SAME config file the CLI is pointed at, exactly as
    // the real proxy does — so the durable half lands where `tcr` looks.
    let manager =
        crate::test_support::live_proxy::serve(listener, config.clone(), Some(path.clone()));

    // Point the CLI at the live port by rewriting the file it will load.
    point_at_port(&path, port);

    set_enabled(&path, "alice@example.com", true).await.unwrap();

    // 1. THE RUNNING ROTATION. Not a fresh Manager, not the file — the process
    //    that would serve the next request.
    let live = manager.snapshot(OffsetDateTime::now_utc());
    assert_eq!(live.accounts[0].name, "alice@example.com");
    assert!(
        live.accounts[0].disabled,
        "the account is parked IN THE SERVING PROCESS, not just on disk"
    );
    assert!(
        !live.accounts[1].disabled,
        "and only the resolved account was touched"
    );

    // 2. …and the file carries it, so the bench survives a restart.
    let after: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        after["accounts"][0]["disabled"],
        serde_json::json!(true),
        "the durable half landed too: {after}"
    );

    // 3. Re-enable, live, and the key is DROPPED from the document — the same
    //    contract the file-only path has always had.
    set_enabled(&path, "alice@example.com", false)
        .await
        .unwrap();
    assert!(
        !manager.snapshot(OffsetDateTime::now_utc()).accounts[0].disabled,
        "re-enable reaches the live rotation too"
    );
    let after: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert!(
        after["accounts"][0].get("disabled").is_none(),
        "re-enable DROPS the key rather than writing false: {after}"
    );
    fs::remove_file(&path).ok();
}

/// An ambiguous query is refused by the SERVER and the CLI does not fall back:
/// the file's own resolution could land on a different row than the one the
/// server was talking about, and a disable applied to the wrong account is
/// worse than none. The live rotation is broken by hand to hold two rows of
/// the same name; the file keeps unique names throughout, which is what
/// makes the byte-identical assertion below mean what it says.
#[tokio::test]
async fn set_enabled_refuses_an_ambiguous_query_without_writing() {
    let (path, listener) = config_on_a_scratch_port("live-ambiguous").await;

    let before = serve_a_fleet_with_two_alices(&path, listener);

    let err = set_enabled(&path, "alice@example.com", true)
        .await
        .expect_err("an ambiguous query must not be applied");
    let text = err.to_string();
    assert!(
        text.contains("ambiguous") && text.matches("alice@example.com").count() >= 2,
        "the refusal names the candidates: {text}"
    );
    assert_eq!(
        before,
        fs::read_to_string(&path).unwrap(),
        "a refused command leaves the config byte-identical"
    );
    fs::remove_file(&path).ok();
}

// --- post_add_account ---------------------------------------------------
