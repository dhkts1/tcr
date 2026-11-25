//! Tests of the file's schema and of what every writer does with an account's
//! `provider`, plus the fixtures the children's tests share.

use super::*;
use std::fs;
use std::path::{Path, PathBuf};

pub(super) const SAMPLE: &str = r#"{
      "proxy": { "port": 3456, "apiKey": "sk-proxy-secret", "customFlag": true },
      "upstream": "https://api.anthropic.com",
      "switchThreshold": 0.9,
      "quotaProbeSeconds": 120,
      "routes": [{ "name": "r1", "match": "*fable*" }],
      "accounts": [
        {
          "name": "acct-a",
          "type": "oauth",
          "accountUuid": "uuid-a",
          "orgName": "Org A",
          "accessToken": "at-a",
          "refreshToken": "rt-a",
          "expiresAt": 1893456000000,
          "priority": 0,
          "models": ["claude-fable-5"]
        }
      ]
    }"#;

#[test]
fn load_parses_known_fields() {
    let config: Config = serde_json::from_str(SAMPLE).unwrap();
    assert_eq!(config.proxy.port, 3456);
    assert_eq!(config.proxy.api_key.as_deref(), Some("sk-proxy-secret"));
    assert_eq!(config.switch_threshold, 0.9);
    assert_eq!(config.accounts.len(), 1);
    assert_eq!(config.accounts[0].name, "acct-a");
    assert_eq!(config.accounts[0].expires_at, Some(1_893_456_000_000));
}

#[test]
fn save_round_trip_preserves_unknown_fields() {
    let config: Config = serde_json::from_str(SAMPLE).unwrap();
    let tmp = std::env::temp_dir().join(format!("tcr-cfg-{}.json", std::process::id()));
    save(&tmp, &config).unwrap();
    let reloaded = fs::read_to_string(&tmp).unwrap();
    let value: Value = serde_json::from_str(&reloaded).unwrap();

    // Unmodelled top-level keys survive.
    assert_eq!(value["quotaProbeSeconds"], serde_json::json!(120));
    assert!(value["routes"].is_array());
    // Unmodelled proxy key survives.
    assert_eq!(value["proxy"]["customFlag"], serde_json::json!(true));
    // Unmodelled per-account key survives.
    assert_eq!(
        value["accounts"][0]["models"],
        serde_json::json!(["claude-fable-5"])
    );

    fs::remove_file(&tmp).ok();
}

/// An account config carrying `groups` survives a load→save round-trip
/// unchanged, and — via [`SAMPLE`], whose one account has no `groups` key —
/// an account without the key still loads (defaulting to `None`/no groups).
#[test]
fn groups_round_trip_and_are_optional() {
    // No `groups` key at all: loads fine, defaults to no groups.
    let config: Config = serde_json::from_str(SAMPLE).unwrap();
    assert_eq!(config.accounts[0].groups, None);
    assert!(!config.accounts[0].in_group("codereview"));

    // A `groups` key present: round-trips byte-for-byte through save→reload.
    let with_groups = r#"{
          "accounts": [
            {
              "name": "acct-grouped",
              "type": "oauth",
              "accessToken": "at-grouped",
              "priority": 0,
              "groups": ["codereview", "burst"]
            }
          ]
        }"#;
    let config: Config = serde_json::from_str(with_groups).unwrap();
    assert_eq!(
        config.accounts[0].groups,
        Some(vec!["codereview".to_string(), "burst".to_string()])
    );
    assert!(config.accounts[0].in_group("codereview"));
    assert!(
        !config.accounts[0].in_group("CodeReview"),
        "matching is case-sensitive"
    );
    assert!(!config.accounts[0].in_group("nope"));

    let tmp = std::env::temp_dir().join(format!("tcr-cfg-groups-{}.json", std::process::id()));
    save(&tmp, &config).unwrap();
    let reloaded: Config = serde_json::from_str(&fs::read_to_string(&tmp).unwrap()).unwrap();
    assert_eq!(reloaded.accounts[0].groups, config.accounts[0].groups);
    fs::remove_file(&tmp).ok();
}

#[test]
fn all_group_names_is_the_union_of_every_accounts_groups() {
    let config: Config = serde_json::from_str(GROUPED_ACCOUNTS_JSON).unwrap();
    let names: Vec<String> = config.all_group_names().into_iter().collect();
    assert_eq!(names, vec!["codereview".to_string(), "dev".to_string()]);
}

#[test]
fn group_colors_covers_every_fleet_group_including_unconfigured_ones() {
    let tmp = std::env::temp_dir().join(format!("tcr-cfg-colors-{}.json", std::process::id()));
    fs::write(&tmp, GROUPED_ACCOUNTS_JSON).unwrap();
    save_group_color(&tmp, "dev", Some("#0a84ff")).unwrap();
    let config: Config = serde_json::from_str(&fs::read_to_string(&tmp).unwrap()).unwrap();

    let colors = config.group_colors();
    assert_eq!(colors.len(), 2, "both codereview and dev are present");
    assert_eq!(colors["dev"], "#0a84ff");
    assert_eq!(colors["codereview"], derive_group_color("codereview"));

    fs::remove_file(&tmp).ok();
}

/// Two accounts, `alice` in `codereview` and `dev`, `bob` in `codereview`
/// only — enough to exercise a shared group and a group with only one
/// member without a third fixture.
pub(super) const GROUPED_ACCOUNTS_JSON: &str = r#"{
      "accounts": [
        { "name": "alice@example.com", "type": "oauth", "accessToken": "at-a",
          "priority": 0, "groups": ["codereview", "dev"] },
        { "name": "bob@example.com", "type": "oauth", "accessToken": "at-b",
          "priority": 1, "groups": ["codereview"] }
      ]
    }"#;

#[test]
fn defaults_apply_to_minimal_config() {
    let config: Config = serde_json::from_str(r#"{ "accounts": [] }"#).unwrap();
    assert_eq!(config.proxy.port, 3456);
    assert_eq!(config.upstream, "https://api.anthropic.com");
    assert_eq!(config.switch_threshold, 0.95);
    // Absent `pacing` key → pacing OFF: no in-flight cap, no min-spacing.
    assert_eq!(config.pacing.max_in_flight_per_account, None);
    assert_eq!(config.pacing.min_spacing_ms, None);
    assert!(!config.pacing.is_active());
    // Absent `http1Only` key → OFF: the serving client stays on h2.
    assert!(!config.http1_only);
}

#[test]
fn http1_only_true_deserializes() {
    let config: Config = serde_json::from_str(r#"{ "accounts": [], "http1Only": true }"#).unwrap();
    assert!(config.http1_only);
}

#[test]
fn default_pacing_ships_off() {
    // Guards the DEFAULT itself, not just deserialization: a per-account
    // concurrency cap costs prompt-cache locality, so it must stay opt-in.
    // If this flips, pacing was silently turned back on for every user.
    let pacing = default_pacing();
    assert_eq!(pacing.max_in_flight_per_account, None);
    assert_eq!(pacing.min_spacing_ms, None);
    assert_eq!(pacing.effective_max_in_flight(), None);
    assert!(!pacing.is_active());
}

#[test]
fn empty_pacing_object_disables_pacing() {
    // `"pacing": {}` spells out what the default already is: both knobs
    // None → inert. Kept so a config that writes the key stays supported.
    let config: Config = serde_json::from_str(r#"{ "accounts": [], "pacing": {} }"#).unwrap();
    assert_eq!(config.pacing.max_in_flight_per_account, None);
    assert_eq!(config.pacing.min_spacing_ms, None);
    assert!(!config.pacing.is_active());
}

#[test]
fn explicit_pacing_overrides_the_default() {
    let config: Config = serde_json::from_str(
        r#"{ "accounts": [], "pacing": { "maxInFlightPerAccount": 5, "minSpacingMs": 200 } }"#,
    )
    .unwrap();
    assert_eq!(config.pacing.max_in_flight_per_account, Some(5));
    assert_eq!(config.pacing.min_spacing_ms, Some(200));
}

#[test]
fn absent_throttle_keys_default_both_buckets_on() {
    // Neither key present → both defaults → both ON. The per-org bucket is the
    // tight one; the fleet ceiling is deliberately looser.
    let config: Config = serde_json::from_str(r#"{ "accounts": [] }"#).unwrap();
    assert!(config.account_throttle.is_active());
    assert_eq!(config.account_throttle.effective_min_spacing(), Some(350));
    assert_eq!(config.account_throttle.effective_burst(), 24);
    assert!(config.fleet_throttle.is_active());
    assert_eq!(config.fleet_throttle.effective_min_spacing(), Some(100));
    assert_eq!(config.fleet_throttle.effective_burst(), 32);
    // The ceiling must admit at least one full per-org burst, or a one-account
    // wave the per-org bucket lets through is held by the fleet bucket instead.
    assert!(
        config.fleet_throttle.effective_burst() > config.account_throttle.effective_burst(),
        "fleet burst {} must exceed per-org burst {}",
        config.fleet_throttle.effective_burst(),
        config.account_throttle.effective_burst()
    );
}

#[test]
fn empty_object_disables_only_that_bucket() {
    // The escape hatch is now PER BUCKET. Disabling one leaves the other at its
    // default — there is no single key that means "unthrottled" any more, which
    // is exactly why migrating a stale `throttle` key branches on whether it
    // was active rather than mapping it onto one new key uniformly.
    let config: Config =
        serde_json::from_str(r#"{ "accounts": [], "accountThrottle": {} }"#).unwrap();
    assert_eq!(config.account_throttle.min_spacing_ms, None);
    assert_eq!(config.account_throttle.burst, None);
    assert!(!config.account_throttle.is_active());
    assert!(
        config.fleet_throttle.is_active(),
        "disabling the per-org bucket must NOT disable the fleet ceiling"
    );
}

#[test]
fn both_empty_objects_disable_throttling_entirely() {
    let config: Config =
        serde_json::from_str(r#"{ "accounts": [], "accountThrottle": {}, "fleetThrottle": {} }"#)
            .unwrap();
    assert!(!config.account_throttle.is_active());
    assert!(!config.fleet_throttle.is_active());
}

#[test]
fn explicit_throttle_knobs_override_defaults() {
    let config: Config = serde_json::from_str(
        r#"{ "accounts": [],
                 "accountThrottle": { "minSpacingMs": 350, "burst": 5 },
                 "fleetThrottle": { "minSpacingMs": 40, "burst": 64 } }"#,
    )
    .unwrap();
    assert_eq!(config.account_throttle.effective_min_spacing(), Some(350));
    assert_eq!(config.account_throttle.effective_burst(), 5);
    assert_eq!(config.fleet_throttle.effective_min_spacing(), Some(40));
    assert_eq!(config.fleet_throttle.effective_burst(), 64);
}

#[test]
fn lock_account_parses_when_present() {
    let config: Config =
        serde_json::from_str(r#"{ "accounts": [], "lockAccount": "acme" }"#).unwrap();
    assert_eq!(config.lock_account, Some("acme".to_string()));
}

#[test]
fn lock_account_absent_defaults_none() {
    let config: Config = serde_json::from_str(r#"{ "accounts": [] }"#).unwrap();
    assert_eq!(config.lock_account, None);
}

#[test]
fn control_account_parses_when_present() {
    let config: Config =
        serde_json::from_str(r#"{ "accounts": [], "controlAccount": "alice@example.com" }"#)
            .unwrap();
    assert_eq!(
        config.control_account,
        Some(ControlAccountRef::Name("alice@example.com".to_string()))
    );
}

/// The identity-object shape an older `tcr` wrote still parses back to the
/// exact fields it carries, `orgUuid` included — nothing writes it now, and
/// a config that has one must still load.
#[test]
fn control_account_parses_identity_object() {
    let config: Config = serde_json::from_str(
        r#"{ "accounts": [], "controlAccount": {
                "name": "alice@example.com",
                "orgUuid": "11111111-1111-1111-1111-111111111111"
            } }"#,
    )
    .unwrap();
    assert_eq!(
        config.control_account,
        Some(ControlAccountRef::Identity {
            name: "alice@example.com".to_string(),
            account_uuid: None,
            org_uuid: Some("11111111-1111-1111-1111-111111111111".to_string()),
            org_name: None,
        })
    );
}

#[test]
fn control_account_absent_defaults_none() {
    let config: Config = serde_json::from_str(r#"{ "accounts": [] }"#).unwrap();
    assert_eq!(config.control_account, None);
}

/// `controlIdentity` reads its three lowercase spellings, defaults to
/// `warn` when absent, and rejects anything else rather than guessing.
#[test]
fn control_identity_parses_lowercase_and_defaults_to_warn() {
    let config: Config = serde_json::from_str(r#"{ "accounts": [] }"#).unwrap();
    assert_eq!(config.control_identity, ControlIdentityMode::Warn);
    for (spelling, expected) in [
        ("refuse", ControlIdentityMode::Refuse),
        ("warn", ControlIdentityMode::Warn),
        ("off", ControlIdentityMode::Off),
    ] {
        let config: Config = serde_json::from_str(&format!(
            r#"{{ "accounts": [], "controlIdentity": "{spelling}" }}"#
        ))
        .unwrap();
        assert_eq!(config.control_identity, expected, "{spelling}");
        assert_eq!(expected.as_str(), spelling);
    }
    assert!(
        serde_json::from_str::<Config>(r#"{ "accounts": [], "controlIdentity": "Refuse" }"#)
            .is_err(),
        "an unknown spelling is an error, not a silent default"
    );
}

/// `skip_serializing_if` round trip: setting then serializing must NOT emit
/// a `null`, only either the string or an absent key — this is what makes
/// `save_control_account`'s clear-removes-the-key contract representable.
#[test]
fn control_account_absent_does_not_serialize_a_null() {
    let config: Config = serde_json::from_str(r#"{ "accounts": [] }"#).unwrap();
    let json = serde_json::to_value(&config).unwrap();
    assert!(
        json.get("controlAccount").is_none(),
        "an absent control account must not serialize at all: {json}"
    );
}

/// A unique temp path per test — the suite runs tests in parallel threads of
/// ONE process, so a pid-only name collides.
pub(super) fn tmp_path(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("tcr-{tag}-{}-{seq}.json", std::process::id()))
}

/// `json` written to a fresh [`tmp_path`] tagged `tag`, and that path.
pub(super) fn tmp_file(tag: &str, json: &str) -> PathBuf {
    let path = tmp_path(tag);
    fs::write(&path, json).unwrap();
    path
}

pub(super) fn read_json(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).expect("read persisted config"))
        .expect("persisted config is valid JSON")
}

/// A file carrying keys the server does not model, plus a second account, so
/// a targeted flag write can be checked for collateral damage.
pub(super) const DISABLE_SAMPLE: &str = r#"{
      "warmupSeconds": 900,
      "pacing": { "maxInFlightPerAccount": 3 },
      "routes": [{ "name": "r1", "match": "*fable*" }],
      "accounts": [
        { "name": "acct-a", "type": "oauth", "accessToken": "at-a",
          "refreshToken": "rt-a", "expiresAt": 1, "models": ["claude-fable-5"] },
        { "name": "acct-b", "type": "oauth", "accessToken": "at-b",
          "refreshToken": "rt-b", "expiresAt": 2 }
      ]
    }"#;

// --- provider: which upstream an account belongs to ---------------------

/// A complete Anthropic OAuth record for the provider tests below, so each
/// test spells out only the fields it is about.
pub(super) fn account_fixture(name: &str) -> Account {
    Account {
        access_token: "at-fixture".to_string(),
        refresh_token: Some("rt-fixture".to_string()),
        expires_at: Some(1_800_000_000_000),
        priority: None,
        ..Account::fixture(name)
    }
}

/// An absent `provider` is Anthropic, which is what every row on disk was
/// before the key existed; `"codex"` reads as Codex; and `codexUpstream`
/// takes its default when the file never names it.
#[test]
fn account_provider_defaults_to_anthropic_and_reads_codex() {
    let doc = r#"{ "accounts": [
            { "name": "alice@example.com", "accessToken": "at-a" },
            { "name": "bob@example.com", "provider": "codex", "accessToken": "at-b",
              "accountUuid": "22222222-2222-4222-8222-222222222222" }
        ] }"#;
    let config: Config = serde_json::from_str(doc).expect("parses");
    assert_eq!(
        config.accounts[0].provider,
        crate::base::provider::Provider::Anthropic
    );
    assert_eq!(
        config.accounts[1].provider,
        crate::base::provider::Provider::Codex
    );
    assert_eq!(config.codex_upstream, "https://chatgpt.com");
}

/// An upgraded Anthropic-only file gains neither a `provider` key on its row
/// nor a top-level `codexUpstream`, whether it is saved back through
/// `save_account`'s in-place update or through `save_settings`, the
/// whole-config writer every mutating `tcr account` verb uses.
///
/// The second save is the one that pins the two `skip_serializing_if`s:
/// `save_account`'s update path edits the row key by key and never
/// serializes `Account` or `Config`. Watched red by dropping either skip:
/// the file then carries `"provider": "anthropic"` or
/// `"codexUpstream": "https://chatgpt.com"`.
///
/// What it pins is "no new keys", not bytes, and bytes could not be pinned:
/// `serde_json` is built without `preserve_order`, so every write sorts keys;
/// the Updated arm stamps `credentialVersion`; and `save_settings` writes
/// defaults that have no skip, such as `switchThreshold`.
#[test]
fn anthropic_only_config_resaves_without_provider_or_codex_upstream() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(crate::base::paths::CONFIG_FILE);
    let original = r#"{
  "proxy": {
    "port": 3999
  },
  "upstream": "https://api.anthropic.com",
  "accounts": [
    {
      "name": "alice@example.com",
      "type": "oauth",
      "accessToken": "at-a",
      "refreshToken": "rt-a",
      "expiresAt": 1789000000000
    }
  ]
}"#;
    fs::write(&path, original).unwrap();
    let (config, _) = load_or_init(&path).expect("loads"); // the loader the server boot uses
    let account = config.accounts[0].clone();
    save_account(&path, &account).expect("saves"); // unchanged row: an in-place Updated
    let after = fs::read_to_string(&path).unwrap();
    assert!(
        !after.contains("provider"),
        "an Anthropic row must write no provider key:\n{after}"
    );
    assert!(
        !after.contains("codexUpstream"),
        "an untouched default must not be materialised:\n{after}"
    );
    // `save_account` above never serializes the struct; the whole-config
    // writer is where either skip is actually exercised.
    save_settings(&path, &config).expect("saves the whole config");
    let after = fs::read_to_string(&path).unwrap();
    assert!(
        !after.contains("provider"),
        "a whole-config save must write no provider key for an Anthropic row:\n{after}"
    );
    assert!(
        !after.contains("codexUpstream"),
        "a whole-config save must not materialise the default codexUpstream:\n{after}"
    );
}

/// An Anthropic-only config re-saves with no `codexHome` key, through the row writer and
/// the whole-config writer alike: the folder is a Codex fact, and an Anthropic row never
/// has one.
///
/// Watched red: serializing the field with no `skip_serializing_if` writes
/// `"codexHome": null` onto alice.
#[test]
fn an_anthropic_only_config_resaves_without_codex_home() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(crate::base::paths::CONFIG_FILE);
    let original = r#"{
  "proxy": {
    "port": 3999
  },
  "accounts": [
    {
      "name": "alice@example.com",
      "type": "oauth",
      "accessToken": "at-a",
      "refreshToken": "rt-a",
      "expiresAt": 1789000000000
    }
  ]
}"#;
    fs::write(&path, original).unwrap();
    let (config, _) = load_or_init(&path).expect("loads");
    assert_eq!(config.accounts[0].codex_home, None);
    assert!(!config.accounts[0].extra.contains_key("codexHome"));
    save_account(&path, &config.accounts[0]).expect("saves the row");
    assert!(
        !fs::read_to_string(&path).unwrap().contains("codexHome"),
        "the row writer adds no codexHome"
    );
    save_settings(&path, &config).expect("saves the whole config");
    assert!(
        !fs::read_to_string(&path).unwrap().contains("codexHome"),
        "the whole-config writer adds no codexHome"
    );
    let mut whole = config;
    whole.accounts[0].access_token = "at-b".to_string();
    save(&path, &whole).expect("writes the whole document");
    assert!(
        !fs::read_to_string(&path).unwrap().contains("codexHome"),
        "nor does a full rewrite"
    );
}

/// A config file in a fresh temp dir holding one Codex row, bob's, imported from
/// `/Users/example/.codex`; the dir (kept alive by the caller), the path and the loaded
/// config.
fn bob_codex_row_with_a_codex_home() -> (tempfile::TempDir, PathBuf, Config) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(crate::base::paths::CONFIG_FILE);
    fs::write(
        &path,
        r#"{ "accounts": [
                { "name": "bob@example.com", "type": "oauth", "provider": "codex",
                  "accountUuid": "55555555-5555-4555-8555-555555555555",
                  "orgUuid": "66666666-6666-4666-8666-666666666666",
                  "accessToken": "at-b", "refreshToken": "rt-b", "expiresAt": 1789000000000,
                  "credentialVersion": 2, "codexHome": "/Users/example/.codex" }
            ] }"#,
    )
    .unwrap();
    let (config, _) = load_or_init(&path).expect("loads");
    (dir, path, config)
}

/// A Codex row's folder loads into its field, not into `extra`, and survives a save and a
/// reload unchanged.
///
/// Watched red: without the field, the key lives in `extra` and the struct reads `None`.
#[test]
fn codex_home_round_trips_on_codex_rows() {
    let (_dir, path, config) = bob_codex_row_with_a_codex_home();
    let row = &config.accounts[0];
    assert_eq!(row.codex_home.as_deref(), Some("/Users/example/.codex"));
    assert!(
        !row.extra.contains_key("codexHome"),
        "a field, not an extra key"
    );
    save(&path, &config).expect("saves");
    let (again, _) = load_or_init(&path).expect("reloads");
    assert_eq!(again.accounts[0].codex_home, row.codex_home);
    let doc: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(doc["accounts"][0]["codexHome"], "/Users/example/.codex");
}

/// The folder is the latest login's, never the first's: a Codex save with a folder writes
/// it over the old one, and a save with none (a login of tcr's own) removes the key, so
/// TcrBar stops offering to re-import from a folder this row no longer came from.
///
/// Watched red: without the overwrite in `merge_account`'s update, both saves leave
/// `/Users/example/.codex` in place.
#[test]
fn a_codex_save_without_codex_home_removes_it() {
    let (_dir, path, config) = bob_codex_row_with_a_codex_home();
    let codex_home_on_disk = |path: &Path| -> Value {
        let doc: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        doc["accounts"][0]
            .get("codexHome")
            .cloned()
            .unwrap_or(Value::Null)
    };

    let moved = Account {
        access_token: "at-b2".to_string(),
        codex_home: Some("/Users/example/.codex-b".to_string()),
        ..config.accounts[0].clone()
    };
    assert!(matches!(
        save_account(&path, &moved).unwrap(),
        AccountWrite::Updated
    ));
    assert_eq!(codex_home_on_disk(&path), "/Users/example/.codex-b");

    let own = Account {
        access_token: "at-b3".to_string(),
        codex_home: None,
        ..config.accounts[0].clone()
    };
    assert!(matches!(
        save_account(&path, &own).unwrap(),
        AccountWrite::Updated
    ));
    assert_eq!(
        codex_home_on_disk(&path),
        Value::Null,
        "a save with no folder removes the key"
    );
}

/// A Codex import for someone who already has an Anthropic row is ADDED
/// beside it, carrying its provider, and the Anthropic credential is left
/// exactly as it was.
#[test]
fn a_codex_row_is_saved_with_its_provider_and_never_matches_an_anthropic_row() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(crate::base::paths::CONFIG_FILE);
    fs::write(&path, r#"{ "accounts": [
            { "name": "alice@example.com", "accessToken": "at-anthropic", "refreshToken": "rt-anthropic" }
        ] }"#).unwrap();
    let mut codex = account_fixture("alice@example.com/codex");
    codex.provider = crate::base::provider::Provider::Codex;
    codex.account_uuid = Some("11111111-1111-4111-8111-111111111111".to_string());
    codex.access_token = "at-codex".to_string();
    codex.refresh_token = Some("rt-codex".to_string());
    let write = save_account(&path, &codex).expect("saves");
    std::assert_matches!(
        write,
        AccountWrite::Added,
        "a Codex row must be ADDED beside the Anthropic row"
    );
    let after: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let rows = after["accounts"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0]["accessToken"], "at-anthropic",
        "the Anthropic row's credential must be untouched"
    );
    assert_eq!(rows[1]["provider"], "codex");
}

/// The Updated arm's provider stamp spells Anthropic as the ABSENT key: a row
/// that says `"provider": "anthropic"` out loud loses the key on the next
/// in-place save, the same file a fresh Anthropic row would have produced.
/// Watched red by deleting the stamp's `remove`.
#[test]
fn save_account_update_drops_an_explicit_anthropic_provider_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(crate::base::paths::CONFIG_FILE);
    fs::write(
        &path,
        r#"{ "accounts": [
                { "name": "alice@example.com", "provider": "anthropic", "type": "oauth",
                  "accessToken": "at-old", "refreshToken": "rt-old" }
            ] }"#,
    )
    .unwrap();

    let write = save_account(&path, &account_fixture("alice@example.com")).expect("saves");
    assert_eq!(write, AccountWrite::Updated);
    let after = read_json(&path);
    let row = &after["accounts"][0];
    assert_eq!(
        row["accessToken"], "at-fixture",
        "the update must have landed on this row"
    );
    assert!(
        row.get("provider").is_none(),
        "an Anthropic row must carry no provider key after an update: {row}"
    );
}

/// The other half of "written only when set": a `codexUpstream` the user DID
/// change survives the whole-config writer, so pointing Codex at a stub or a
/// regional host is not silently reverted by the next `tcr account` verb.
#[test]
fn a_non_default_codex_upstream_survives_save_settings() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(crate::base::paths::CONFIG_FILE);
    fs::write(
        &path,
        r#"{ "codexUpstream": "https://codex-stub.example.com", "accounts": [] }"#,
    )
    .unwrap();
    let (config, _) = load_or_init(&path).expect("loads");
    assert_eq!(config.codex_upstream, "https://codex-stub.example.com");

    save_settings(&path, &config).expect("saves the whole config");
    assert_eq!(
        read_json(&path)["codexUpstream"],
        "https://codex-stub.example.com",
        "a user-set codexUpstream must be written back"
    );
    let (reloaded, _) = load_or_init(&path).expect("reloads");
    assert_eq!(reloaded.codex_upstream, "https://codex-stub.example.com");
}

#[test]
fn throttle_zero_spacing_is_inert() {
    // `Some(0)` spacing normalizes to unset (footgun parity with pacing).
    let config: Config =
        serde_json::from_str(r#"{ "accounts": [], "accountThrottle": { "minSpacingMs": 0 } }"#)
            .unwrap();
    assert_eq!(config.account_throttle.effective_min_spacing(), None);
    assert!(!config.account_throttle.is_active());
}

/// The migration lives in [`load`], not in `Deserialize`, because `Config`
/// carries a `#[serde(flatten)] extra` catch-all that would otherwise absorb a
/// stale `throttle` key in silence and run on defaults. Every production caller
/// goes through `load`; the only non-test `from_value::<Config>` is inside it.
/// A stale `throttle` key used to be a hard `load` error — see
/// `legacy_throttle_key_migrates_to_account_throttle` and its siblings above
/// for the auto-migration this replaced it with; this test only pins that the
/// already-renamed shape (no `throttle` key at all) keeps working.
#[test]
fn renamed_throttle_key_loads_without_migration() {
    let path = tmp_path("throttle-already-renamed");
    fs::write(
        &path,
        r#"{ "accounts": [], "accountThrottle": { "minSpacingMs": 350, "burst": 4 } }"#,
    )
    .unwrap();
    let ok = load(&path).expect("the renamed key must load");
    assert_eq!(ok.account_throttle.effective_burst(), 4);
    assert!(!ok.migrated_legacy_throttle);

    fs::remove_file(&path).ok();
}
