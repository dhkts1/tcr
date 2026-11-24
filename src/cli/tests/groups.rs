//! Tests for the group verbs in `cli/groups.rs`: add and remove, reserve, park, control
//! opt-in, color, and the `group ls` text and JSON.

use super::*;
/// Three accounts: alice in two groups, bob in one (shared with alice),
/// carol in none — enough to cover multi-group membership, a shared
/// group, and an ungrouped account in one fixture. Carries `routes` (an
/// unmodelled top-level key) and per-account `models` (an unmodelled
/// per-account key) so a mutation's flatten round-trip can be checked
/// without a second fixture.
const GROUPED_ACCOUNTS: &str = r#"{
      "proxy": { "port": 3456 },
      "quotaProbeSeconds": 120,
      "routes": [{ "name": "r1", "match": "*fable*" }],
      "accounts": [
        { "name": "alice@example.com", "type": "oauth", "orgName": "Org A",
          "orgUuid": "uuid-a", "accessToken": "at-a", "refreshToken": "rt-a",
          "expiresAt": 1893456000000, "priority": 0,
          "groups": ["codereview", "dev"], "models": ["opus"] },
        { "name": "bob@example.com", "type": "oauth", "orgName": "Org B",
          "orgUuid": "uuid-b", "accessToken": "at-b", "refreshToken": "rt-b",
          "expiresAt": 1893456000000, "priority": 1,
          "groups": ["codereview"] },
        { "name": "carol@example.com", "type": "oauth", "orgName": "Org C",
          "orgUuid": "uuid-c", "accessToken": "at-c", "refreshToken": "rt-c",
          "expiresAt": 1893456000000, "priority": 2 }
      ]
    }"#;

/// The fleet's real shape: one email logged into two orgs. `tcr group add`
/// used to take the FIRST match and report success, so the panel labelled a
/// row nobody asked for and had no way to notice.
const DUPLICATED_EMAIL: &str = r#"{
      "accounts": [
        { "name": "henry@example.com", "type": "oauth", "orgName": "Personal",
          "orgUuid": "11111111-1111-1111-1111-111111111111",
          "accountUuid": "uuid-person", "accessToken": "at-p",
          "refreshToken": "rt-p", "expiresAt": 1893456000000, "priority": 0 },
        { "name": "henry@example.com", "type": "oauth", "orgName": "Example Team",
          "orgUuid": "22222222-2222-2222-2222-222222222222",
          "accountUuid": "uuid-person", "accessToken": "at-t",
          "refreshToken": "rt-t", "expiresAt": 1893456000000, "priority": 1 }
      ]
    }"#;

/// The bare email names ONE row after the loader's migration — the Personal
/// one, which keeps it — and the Team row is addressed by the name that was
/// minted for it. The assertion is on the OTHER row every time: labelling
/// the right account is only half of it, and a resolver that labelled both
/// would pass a check that only looked at the one it was asked for.
#[test]
fn group_add_labels_exactly_the_named_row_of_a_migrated_duplicate() {
    let path = write_config("group-add-migrated", DUPLICATED_EMAIL);
    add_to_group(&path, "codereview", "henry@example.com/example-team").unwrap();

    let config = load(&path);
    assert_eq!(config.accounts[0].name, "henry@example.com");
    assert_eq!(config.accounts[1].name, "henry@example.com/example-team");
    assert_eq!(
        config.accounts[0].groups, None,
        "the Personal row was not asked for and must not be labelled"
    );
    assert_eq!(
        config.accounts[1].groups,
        Some(vec!["codereview".to_string()]),
        "the Team row is the one the name pointed at"
    );

    add_to_group(&path, "burst", "henry@example.com").unwrap();
    let config = load(&path);
    assert_eq!(
        config.accounts[0].groups,
        Some(vec!["burst".to_string()]),
        "the bare email is the Personal row and nothing else"
    );
    assert_eq!(
        config.accounts[1].groups,
        Some(vec!["codereview".to_string()]),
        "the Team row keeps only its own label"
    );
}

/// A duplicate put back by hand is MIGRATED AGAIN on the next load, not
/// refused and not guessed at. That is what makes uniqueness a property of
/// the system rather than of one upgrade: every verb goes through
/// `config::load`, so nothing downstream ever sees two rows of one name.
///
/// The original regression this replaces: an exact-name scan silently took
/// `accounts[0]`, so the operator saw "Added ... to group" about the wrong
/// account. Here the label lands on the row the name resolves to, and the
/// re-minted name is on disk for the next reader.
#[test]
fn a_hand_edited_duplicate_is_migrated_again_on_the_next_load() {
    let path = write_config("group-add-rebroken", DUPLICATED_EMAIL);
    assert_eq!(
        load(&path).accounts[1].name,
        "henry@example.com/example-team"
    );
    rename_account_in_file(&path, 1, "henry@example.com");

    add_to_group(&path, "codereview", "henry@example.com").unwrap();

    let config = load(&path);
    assert_eq!(config.accounts[0].name, "henry@example.com");
    assert_eq!(
        config.accounts[1].name, "henry@example.com/example-team",
        "the re-broken row is renamed again"
    );
    assert_eq!(
        config.accounts[0].groups,
        Some(vec!["codereview".to_string()]),
        "the label landed on the row the bare email names"
    );
    assert_eq!(config.accounts[1].groups, None);
}

/// `rm` resolves the same way — the panel's "remove from group" is the same
/// menu as "add to group" and must not be able to strip the wrong row.
#[test]
fn group_rm_strips_exactly_the_named_row_of_a_migrated_duplicate() {
    let path = write_config("group-rm-migrated", DUPLICATED_EMAIL);
    let team = "henry@example.com/example-team";
    add_to_group(&path, "codereview", team).unwrap();
    add_to_group(&path, "codereview", "henry@example.com").unwrap();

    remove_from_group(&path, "codereview", Some("henry@example.com"), false).unwrap();
    let config = load(&path);
    // Removing the last label DROPS the `groups` key rather than leaving an
    // empty array — `save_group_membership`'s contract, same shape as
    // `disabled == false` removing its key.
    assert_eq!(config.accounts[0].groups, None);
    assert_eq!(
        config.accounts[1].groups,
        Some(vec!["codereview".to_string()]),
        "the row nobody named keeps its label"
    );

    remove_from_group(&path, "codereview", Some(team), false).unwrap();
    assert_eq!(load(&path).accounts[1].groups, None);
}

/// Rewrite one `accounts[]` entry's `name` in the file directly, bypassing
/// `config::load`. The only way to reach a duplicate name now, and therefore
/// the only way to test the refusals that guard one.
fn rename_account_in_file(path: &Path, index: usize, name: &str) {
    let mut doc: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(path).expect("readable"))
            .expect("the fixture is JSON");
    doc["accounts"][index]["name"] = serde_json::Value::String(name.to_string());
    fs::write(
        path,
        serde_json::to_string_pretty(&doc).expect("serializes"),
    )
    .expect("writable");
}

/// An unknown name still comes back with the whole roster, which is what
/// the panel needs to act on — it shells out blind. A plain "no account
/// matches" would be a worse error in exactly the place it is read.
#[test]
fn group_add_names_every_configured_account_when_the_name_is_unknown() {
    let path = write_config("group-add-unknown", TWO_ACCOUNTS);
    let err = add_to_group(&path, "codereview", "nobody@example.com")
        .expect_err("an unknown account is an error");
    let text = err.to_string();
    assert!(text.contains("alice@example.com"), "{text}");
    assert!(text.contains("bob@example.com"), "{text}");
}

#[test]
fn group_add_labels_named_account_and_leaves_siblings_byte_identical() {
    let path = write_config("group-add", TWO_ACCOUNTS);
    add_to_group(&path, "codereview", "alice@example.com").unwrap();
    let config = load(&path);
    assert_eq!(
        config.accounts[0].groups,
        Some(vec!["codereview".to_string()])
    );
    // bob is untouched — same identity fields, still no `groups` key.
    assert_eq!(config.accounts[1].name, "bob@example.com");
    assert_eq!(config.accounts[1].groups, None);
    let raw = fs::read_to_string(&path).unwrap();
    let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert!(
        value["accounts"][1].get("groups").is_none(),
        "bob's raw JSON entry must not gain a groups key: {value}"
    );
    fs::remove_file(&path).ok();
}

#[test]
fn group_add_twice_is_idempotent() {
    let path = write_config("group-add-twice", TWO_ACCOUNTS);
    add_to_group(&path, "codereview", "alice@example.com").unwrap();
    add_to_group(&path, "codereview", "alice@example.com").unwrap();
    let config = load(&path);
    assert_eq!(
        config.accounts[0].groups,
        Some(vec!["codereview".to_string()]),
        "adding the same label twice must not duplicate it"
    );
    fs::remove_file(&path).ok();
}

#[test]
fn group_rm_removes_only_named_label_leaving_others_intact() {
    let path = write_config("group-rm-one", GROUPED_ACCOUNTS);
    remove_from_group(&path, "codereview", Some("alice@example.com"), false).unwrap();
    let config = load(&path);
    assert_eq!(config.accounts[0].groups, Some(vec!["dev".to_string()]));
    // bob still carries codereview — only alice's membership changed.
    assert_eq!(
        config.accounts[1].groups,
        Some(vec!["codereview".to_string()])
    );
    fs::remove_file(&path).ok();
}

#[test]
fn group_rm_all_clears_label_from_every_member_and_nothing_else() {
    let path = write_config("group-rm-all", GROUPED_ACCOUNTS);
    remove_from_group(&path, "codereview", None, true).unwrap();
    let config = load(&path);
    // alice keeps `dev`, loses `codereview`.
    assert_eq!(config.accounts[0].groups, Some(vec!["dev".to_string()]));
    // bob had only `codereview` — the key drops entirely (not `[]`).
    assert_eq!(config.accounts[1].groups, None);
    // carol was never in it and stays untouched.
    assert_eq!(config.accounts[2].groups, None);
    // No other field moved — priorities and identities intact.
    assert_eq!(config.accounts[0].priority, Some(0));
    assert_eq!(config.accounts[1].priority, Some(1));
    assert_eq!(config.accounts[2].priority, Some(2));
    fs::remove_file(&path).ok();
}

#[test]
fn group_rm_all_on_a_group_with_no_members_is_a_success_no_op() {
    let path = write_config("group-rm-all-empty", GROUPED_ACCOUNTS);
    let before = fs::read_to_string(&path).unwrap();
    remove_from_group(&path, "nonexistent-group", None, true).unwrap();
    let after = fs::read_to_string(&path).unwrap();
    assert_eq!(
        before, after,
        "an --all on an empty group must write nothing"
    );
    fs::remove_file(&path).ok();
}

#[test]
fn group_add_unknown_account_errors_and_names_the_configured_accounts() {
    let path = write_config("group-add-unknown", GROUPED_ACCOUNTS);
    let err = add_to_group(&path, "codereview", "nobody@example.com").unwrap_err();
    let message = err.to_string();
    for name in ["alice@example.com", "bob@example.com", "carol@example.com"] {
        assert!(
            message.contains(name),
            "unknown-account error must name every configured account, missing {name}: {message}"
        );
    }
    fs::remove_file(&path).ok();
}

#[test]
fn group_rm_unknown_account_errors_and_names_the_configured_accounts() {
    let path = write_config("group-rm-unknown", GROUPED_ACCOUNTS);
    let err =
        remove_from_group(&path, "codereview", Some("nobody@example.com"), false).unwrap_err();
    let message = err.to_string();
    for name in ["alice@example.com", "bob@example.com", "carol@example.com"] {
        assert!(message.contains(name), "missing {name}: {message}");
    }
    fs::remove_file(&path).ok();
}

#[test]
fn group_add_rejects_bad_label_but_rm_still_accepts_it() {
    // A control character somehow already in the config (hand-edited, or
    // written by an older/buggy version) — `rm` must be able to strip it.
    let with_bad_label = r#"{
          "proxy": { "port": 3456 },
          "accounts": [
            { "name": "alice@example.com", "type": "oauth",
              "accessToken": "at-a", "priority": 0,
              "groups": ["good", "bad\u0000label"] }
          ]
        }"#;
    let bad_label = "bad\u{0}label";

    let path = write_config("group-add-bad-label", with_bad_label);
    let err = add_to_group(&path, bad_label, "alice@example.com").unwrap_err();
    assert!(
        err.to_string().contains("control character"),
        "add must name the character class at fault: {err}"
    );
    // Nothing was written by the rejected `add`.
    let config = load(&path);
    assert_eq!(
        config.accounts[0].groups,
        Some(vec!["good".to_string(), bad_label.to_string()])
    );

    // `rm` does NOT run the validator — it must still remove the bad label.
    remove_from_group(&path, bad_label, Some("alice@example.com"), false).unwrap();
    let config = load(&path);
    assert_eq!(config.accounts[0].groups, Some(vec!["good".to_string()]));
    fs::remove_file(&path).ok();
}

#[test]
fn group_mutation_preserves_unmodelled_extra_keys() {
    let path = write_config("group-extra", GROUPED_ACCOUNTS);
    add_to_group(&path, "burst", "carol@example.com").unwrap();
    let raw = fs::read_to_string(&path).unwrap();
    let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
    // Top-level unmodelled key.
    assert!(
        value["routes"].is_array(),
        "top-level `routes` must survive: {value}"
    );
    assert_eq!(value["quotaProbeSeconds"], serde_json::json!(120));
    // Per-account unmodelled key on an entry NOT even touched by this edit.
    assert_eq!(value["accounts"][0]["models"], serde_json::json!(["opus"]));
    fs::remove_file(&path).ok();
}

// --- group reserve / unreserve (the safety rail) -----------------------

/// Every account belongs to `codereview` and none are disabled — reserving
/// it would leave 0 unreserved enabled accounts, so this fixture is the
/// one the safety rail's hard refusal must fire on.
const ALL_IN_ONE_GROUP: &str = r#"{
      "accounts": [
        { "name": "alice@example.com", "type": "oauth", "accessToken": "at-a",
          "priority": 0, "groups": ["codereview"] },
        { "name": "bob@example.com", "type": "oauth", "accessToken": "at-b",
          "priority": 1, "groups": ["codereview"] }
      ]
    }"#;

/// Bridge test #6: `reserve` refuses when it would leave zero unreserved
/// enabled accounts, and the config is BYTE-IDENTICAL after the refusal —
/// the safety rail runs its count-before-write, not a write-then-rollback.
#[test]
fn reserve_refuses_when_it_would_leave_zero_unreserved_enabled_accounts() {
    let path = write_config("reserve-zero", ALL_IN_ONE_GROUP);
    let before = fs::read_to_string(&path).unwrap();
    let result = reserve_group(&path, "codereview");
    assert!(
        result.is_err(),
        "reserving the fleet's only group must be refused"
    );
    let after = fs::read_to_string(&path).unwrap();
    assert_eq!(
        before, after,
        "a refused reservation must not touch the file at all"
    );
    let config = load(&path);
    assert!(
        !config.is_group_reserved("codereview"),
        "config in memory must also show the group as still unreserved"
    );
    fs::remove_file(&path).ok();
}

/// A disabled account does not count toward the pool either direction —
/// reserving a group that leaves only a DISABLED account unreserved is
/// refused exactly like leaving none at all, and a group with a healthy
/// unreserved sibling succeeds and reports the right remaining count.
#[test]
fn reserve_counts_only_enabled_accounts_in_the_remaining_pool() {
    let path = write_config(
        "reserve-disabled-sibling",
        r#"{
              "accounts": [
                { "name": "alice@example.com", "type": "oauth", "accessToken": "at-a",
                  "priority": 0, "groups": ["codereview"] },
                { "name": "bob@example.com", "type": "oauth", "accessToken": "at-b",
                  "priority": 1, "disabled": true }
              ]
            }"#,
    );
    let result = reserve_group(&path, "codereview");
    assert!(
        result.is_err(),
        "the only OTHER account is disabled — reserving must still refuse"
    );
    fs::remove_file(&path).ok();
}

/// The success path: a group with headroom reserves cleanly, persists,
/// and survives a restart-style reload; unreserve clears it again. Idempotent
/// both ways.
#[test]
fn reserve_then_unreserve_round_trips_and_is_idempotent() {
    let path = write_config("reserve-roundtrip", GROUPED_ACCOUNTS);
    reserve_group(&path, "codereview").unwrap();
    let config = load(&path);
    assert!(config.is_group_reserved("codereview"));
    assert!(
        !config.is_group_reserved("dev"),
        "only the named group changes"
    );

    // Idempotent: reserving an already-reserved group succeeds.
    reserve_group(&path, "codereview").unwrap();
    assert!(load(&path).is_group_reserved("codereview"));

    unreserve_group(&path, "codereview").unwrap();
    let config = load(&path);
    assert!(!config.is_group_reserved("codereview"));

    // Idempotent the other way too.
    unreserve_group(&path, "codereview").unwrap();
    assert!(!load(&path).is_group_reserved("codereview"));

    fs::remove_file(&path).ok();
}

/// `tcr group reserve` rejects the same bad-character labels `tcr group
/// add` already does — reuses [`validate_group_label_chars`], no second
/// validator.
#[test]
fn reserve_rejects_bad_label() {
    let path = write_config("reserve-bad-label", TWO_ACCOUNTS);
    let result = reserve_group(&path, "bad\nlabel");
    assert!(result.is_err(), "a newline in the label must be rejected");
    fs::remove_file(&path).ok();
}

// --- group reserve in a fleet holding Codex accounts ----------------------

/// alice is the only Claude account, and she is in `orchard`; bob and dana
/// are enabled Codex accounts in no group.
const ONE_CLAUDE_TWO_CODEX: &str = r#"{
      "accounts": [
        { "name": "alice@example.com", "type": "oauth", "accessToken": "at-a",
          "groups": ["orchard"] },
        { "name": "bob@example.com/codex", "provider": "codex", "type": "oauth",
          "accessToken": "at-b", "credentialVersion": 2 },
        { "name": "dana@example.com/codex", "provider": "codex", "type": "oauth",
          "accessToken": "at-d", "credentialVersion": 2 }
      ]
    }"#;

/// `orchard` holds alice (Claude) and bob (Codex); carol (Claude) and dana
/// (Codex) are in no group.
const ORCHARD_HOLDS_CODEX: &str = r#"{
      "accounts": [
        { "name": "alice@example.com", "type": "oauth", "accessToken": "at-a",
          "groups": ["orchard"] },
        { "name": "carol@example.com", "type": "oauth", "accessToken": "at-c" },
        { "name": "bob@example.com/codex", "provider": "codex", "type": "oauth",
          "accessToken": "at-b", "credentialVersion": 2, "groups": ["orchard"] },
        { "name": "dana@example.com/codex", "provider": "codex", "type": "oauth",
          "accessToken": "at-d", "credentialVersion": 2 }
      ]
    }"#;

/// The rail counts Claude accounts: reserving the group that holds the only
/// Claude account leaves two Codex accounts unreserved, and they cannot serve
/// a Claude session, so it is refused. `--force` does not lift it, and the
/// refusal leaves the file byte-identical.
#[test]
fn reserving_the_last_claude_account_counts_claude_only() {
    let refusal = "reserving 'orchard' would leave 0 unreserved enabled Claude accounts — Claude could serve no unrequested traffic, and the 2 Codex account(s) left cannot serve it. every enabled Claude account belongs to this group. Nothing changed.";
    let path = write_config("reserve-last-claude", ONE_CLAUDE_TWO_CODEX);
    let config = load(&path);
    assert_eq!(
        plan_reserve(&config, "orchard", false),
        Err(refusal.to_string())
    );
    assert_eq!(
        plan_reserve(&config, "orchard", true),
        Err(refusal.to_string()),
        "--force lifts only the Codex-members refusal, never the Claude rail"
    );

    let before = fs::read(&path).unwrap();
    let err = reserve_group_with(&path, "orchard", true).expect_err("the rail refuses");
    assert_eq!(err.to_string(), refusal);
    assert_eq!(fs::read(&path).unwrap(), before, "nothing was written");
    fs::remove_file(&path).ok();
}

/// A group holding a Codex account is refused without `--force`, because a
/// Codex session cannot ask for a group: while it is reserved, its Codex
/// members serve nothing. The remedy names the first of them.
#[test]
fn reserving_a_group_with_codex_members_needs_force() {
    let refusal = "reserving 'orchard' strands its Codex account(s) bob@example.com/codex: a Codex session cannot ask for a group, so while 'orchard' is reserved they serve nothing. Remove them first (`tcr group rm orchard bob@example.com/codex`), or pass --force to reserve it anyway. Nothing changed.";
    let path = write_config("reserve-codex-members", ORCHARD_HOLDS_CODEX);
    assert_eq!(
        plan_reserve(&load(&path), "orchard", false),
        Err(refusal.to_string())
    );

    let before = fs::read(&path).unwrap();
    let err = reserve_group_with(&path, "orchard", false).expect_err("refused without --force");
    assert_eq!(err.to_string(), refusal);
    assert_eq!(fs::read(&path).unwrap(), before, "nothing was written");
    fs::remove_file(&path).ok();
}

/// `--force` reserves it and says, per provider, what is left: one Claude
/// account and one Codex account stay unreserved, and one Claude account is
/// few enough to warn about. The already-reserved line ends with the same
/// clause, and the Codex-members refusal still applies to a group that is
/// already reserved.
#[test]
fn force_reserve_warns_and_counts_per_provider() {
    let path = write_config("reserve-force", ORCHARD_HOLDS_CODEX);
    let plan = plan_reserve(&load(&path), "orchard", true).expect("--force reserves it");
    assert_eq!(
            plan.force_warning,
            Some("warning: 'orchard' holds the Codex account(s) bob@example.com/codex. A Codex session cannot ask for a group, so while 'orchard' is reserved they serve nothing — `tcr group rm orchard bob@example.com/codex` returns one to the pool.".to_string())
        );
    assert_eq!(
        plan.reserved_line,
        "group 'orchard' reserved. 1 unreserved enabled Claude account(s) and 1 Codex account(s) remain."
    );
    assert_eq!(
        plan.already_line,
        "group 'orchard' is already reserved; nothing changed. 1 unreserved enabled Claude account(s) and 1 Codex account(s) remain."
    );
    assert_eq!(
            plan.low_count_warning,
            Some("warning: only 1 unreserved enabled Claude account(s) remain — ordinary (unrequested) Claude traffic may starve.".to_string())
        );

    reserve_group_with(&path, "orchard", true).expect("--force writes the reservation");
    assert!(load(&path).is_group_reserved("orchard"));

    let before = fs::read(&path).unwrap();
    assert!(
        reserve_group_with(&path, "orchard", false).is_err(),
        "an already-reserved group holding a Codex account still needs --force"
    );
    assert_eq!(fs::read(&path).unwrap(), before, "nothing was written");
    reserve_group_with(&path, "orchard", true).expect("and with it, it is already reserved");
    fs::remove_file(&path).ok();
}

/// A fleet with no Codex account keeps every word the reserve verb printed
/// before Codex accounts existed, and `--force` changes nothing in it.
#[test]
fn a_claude_only_fleets_reserve_sentence_is_unchanged() {
    let zero_path = write_config("reserve-claude-only-zero", ALL_IN_ONE_GROUP);
    let all_in_one = load(&zero_path);
    let refusal = "reserving 'codereview' would leave 0 unreserved enabled accounts — the fleet could serve no unrequested traffic. every enabled account belongs to this group. Nothing changed.";
    assert_eq!(
        plan_reserve(&all_in_one, "codereview", false),
        Err(refusal.to_string())
    );
    assert_eq!(
        plan_reserve(&all_in_one, "codereview", true),
        Err(refusal.to_string())
    );

    let grouped_path = write_config("reserve-claude-only", GROUPED_ACCOUNTS);
    let grouped = load(&grouped_path);
    let plan = plan_reserve(&grouped, "codereview", false).expect("carol stays unreserved");
    assert_eq!(
            plan,
            ReservePlan {
                force_warning: None,
                reserved_line: "group 'codereview' reserved. 1 unreserved enabled account(s) remain."
                    .to_string(),
                already_line: "group 'codereview' is already reserved; nothing changed. 1 unreserved enabled account(s) remain."
                    .to_string(),
                low_count_warning: Some(
                    "warning: only 1 unreserved enabled account(s) remain — ordinary (unrequested) traffic may starve."
                        .to_string()
                ),
            }
        );
    assert_eq!(plan_reserve(&grouped, "codereview", true), Ok(plan));
    fs::remove_file(&zero_path).ok();
    fs::remove_file(&grouped_path).ok();
}

// --- group park / unpark ------------------------------------------------

/// Mirrors [`reserve_then_unreserve_round_trips_and_is_idempotent`]:
/// parking persists, is scoped to the named group, survives a reload, and
/// both directions are idempotent. Unlike `reserve` there is no
/// pool-floor rail to trip, so the success path is the whole story.
#[test]
fn park_then_unpark_round_trips_and_is_idempotent() {
    let path = write_config("park-roundtrip", GROUPED_ACCOUNTS);
    park_group(&path, "codereview").unwrap();
    let config = load(&path);
    assert!(config.is_group_parked("codereview"));
    assert!(
        !config.is_group_parked("dev"),
        "only the named group changes"
    );

    // Idempotent: parking an already-parked group succeeds.
    park_group(&path, "codereview").unwrap();
    assert!(load(&path).is_group_parked("codereview"));

    unpark_group(&path, "codereview").unwrap();
    assert!(!load(&path).is_group_parked("codereview"));

    // Idempotent the other way too.
    unpark_group(&path, "codereview").unwrap();
    assert!(!load(&path).is_group_parked("codereview"));

    fs::remove_file(&path).ok();
}

/// The two flags COMPOSE and neither write disturbs the other: a group can
/// be parked and reserved at once, and clearing one leaves the other
/// standing. Both were decided up front as independent, so this is the test
/// that keeps them that way.
#[test]
fn park_and_reserve_are_independent_flags() {
    let path = write_config("park-and-reserve", GROUPED_ACCOUNTS);
    reserve_group(&path, "codereview").unwrap();
    park_group(&path, "codereview").unwrap();
    let config = load(&path);
    assert!(config.is_group_reserved("codereview"));
    assert!(config.is_group_parked("codereview"));

    unpark_group(&path, "codereview").unwrap();
    let config = load(&path);
    assert!(
        config.is_group_reserved("codereview"),
        "unparking must not clear the reservation"
    );
    assert!(!config.is_group_parked("codereview"));

    park_group(&path, "codereview").unwrap();
    unreserve_group(&path, "codereview").unwrap();
    let config = load(&path);
    assert!(
        config.is_group_parked("codereview"),
        "unreserving must not clear the park"
    );
    assert!(!config.is_group_reserved("codereview"));

    fs::remove_file(&path).ok();
}

/// Unparking is not a re-enable: a member the operator disabled by hand is
/// still disabled afterwards. The two facts live in different places in the
/// file (an account's `disabled`, a group's `parked`), and this pins that
/// `unpark`'s targeted write never reaches across.
#[test]
fn unparking_leaves_a_members_own_disabled_alone() {
    let path = write_config("park-keeps-disabled", GROUPED_ACCOUNTS);
    let alice = load(&path)
        .accounts
        .into_iter()
        .find(|a| a.name == "alice@example.com")
        .expect("alice is in the fixture");
    config::save_disabled(&path, &alice, true).expect("bench alice by hand");
    park_group(&path, "codereview").unwrap();
    unpark_group(&path, "codereview").unwrap();

    let config = load(&path);
    let alice = config
        .accounts
        .iter()
        .find(|a| a.name == "alice@example.com")
        .expect("alice survives the round trip");
    assert_eq!(
        alice.disabled,
        Some(true),
        "a member disabled on its own stays disabled after unpark"
    );
    fs::remove_file(&path).ok();
}

/// `tcr group park` rejects the same bad-character labels `tcr group
/// reserve`/`add` already do — reuses [`validate_group_label_chars`], no
/// second validator. `unpark` deliberately does NOT validate, so a label
/// that somehow reached the file can always be cleared.
#[test]
fn park_rejects_bad_label_and_unpark_does_not() {
    let path = write_config("park-bad-label", TWO_ACCOUNTS);
    assert!(
        park_group(&path, "bad\nlabel").is_err(),
        "a newline in the label must be rejected"
    );
    assert!(
        unpark_group(&path, "bad\nlabel").is_ok(),
        "unpark must be able to clear a label park would refuse to write"
    );
    fs::remove_file(&path).ok();
}

/// Both `ls` surfaces carry the flag, and they agree.
#[test]
fn ls_reports_a_parked_group_on_both_surfaces() {
    let path = write_config("ls-parked", GROUPED_ACCOUNTS);
    park_group(&path, "codereview").unwrap();
    let config = load(&path);

    let codereview = group_line(&render_groups_text(&config), "codereview");
    assert!(
        codereview.contains("parked=yes"),
        "text ls must mark the parked group: {codereview}"
    );
    let dev = group_line(&render_groups_text(&config), "dev");
    assert!(
        dev.contains("parked=no"),
        "an ordinary group must not be flagged parked: {dev}"
    );

    let json: serde_json::Value =
        serde_json::from_str(&render_groups_json(&config).unwrap()).unwrap();
    let rows = json["groups"].as_array().expect("groups array");
    let row = rows
        .iter()
        .find(|r| r["group"] == "codereview")
        .expect("codereview row");
    assert_eq!(row["parked"], serde_json::json!(true));
    let plain = rows.iter().find(|r| r["group"] == "dev").expect("dev row");
    assert_eq!(plain["parked"], serde_json::json!(false));

    fs::remove_file(&path).ok();
}

// --- allow-control / disallow-control -----------------------------------

/// Mirrors [`reserve_then_unreserve_round_trips_and_is_idempotent`]: opting
/// in persists, is scoped to the named group, survives a reload, and both
/// directions are idempotent.
#[test]
fn allow_then_disallow_control_round_trips_and_is_idempotent() {
    let path = write_config("allow-control-roundtrip", GROUPED_ACCOUNTS);
    allow_control_group(&path, "codereview").unwrap();
    let config = load(&path);
    assert!(config.group_allows_control("codereview"));
    assert!(
        !config.group_allows_control("dev"),
        "only the named group changes"
    );

    // Idempotent: opting in an already-opted-in group succeeds.
    allow_control_group(&path, "codereview").unwrap();
    assert!(load(&path).group_allows_control("codereview"));

    disallow_control_group(&path, "codereview").unwrap();
    let config = load(&path);
    assert!(!config.group_allows_control("codereview"));

    // Idempotent the other way too.
    disallow_control_group(&path, "codereview").unwrap();
    assert!(!load(&path).group_allows_control("codereview"));

    fs::remove_file(&path).ok();
}

/// `tcr group allow-control` rejects the same bad-character labels `tcr
/// group reserve`/`add` already do — reuses [`validate_group_label_chars`].
#[test]
fn allow_control_rejects_bad_label() {
    let path = write_config("allow-control-bad-label", TWO_ACCOUNTS);
    let result = allow_control_group(&path, "bad\nlabel");
    assert!(result.is_err(), "a newline in the label must be rejected");
    fs::remove_file(&path).ok();
}

/// `ls` must show a control-only group as routable once it has opted in —
/// the whole point of the flag, on the surface an operator actually reads.
#[test]
fn ls_marks_an_opted_in_control_only_group_as_routing() {
    let path = write_config("routes-control-allowed", CONTROL_ONLY_GROUP);
    allow_control_group(&path, "research").unwrap();
    let config = load(&path);
    let research = group_line(&render_groups_text(&config), "research");
    assert!(
        research.contains("allows_control=yes")
            && research.contains("routes=yes")
            && !research.contains("route_block="),
        "an opted-in control-only group must report routable: {research}"
    );
    let dev = group_line(&render_groups_text(&config), "dev");
    assert!(
        dev.contains("allows_control=no"),
        "an ordinary group must not be flagged as opted in: {dev}"
    );
    fs::remove_file(&path).ok();
}

/// Same fact on the JSON surface.
#[test]
fn ls_json_carries_allows_control() {
    let path = write_config("routes-control-allowed-json", CONTROL_ONLY_GROUP);
    allow_control_group(&path, "research").unwrap();
    let config = load(&path);
    let value: serde_json::Value =
        serde_json::from_str(&render_groups_json(&config).unwrap()).unwrap();
    let row = value["groups"]
        .as_array()
        .expect("groups array")
        .iter()
        .find(|r| r["group"] == "research")
        .expect("research row");
    assert_eq!(row["allowsControl"], serde_json::json!(true));
    assert_eq!(row["routes"], serde_json::json!(true));
    assert_eq!(row["routeBlock"], serde_json::Value::Null);
    fs::remove_file(&path).ok();
}

// --- unroutable groups ---------------------------------------------------

/// A fleet where the control account is the ONLY member of a group. This is
/// the shape that shipped on the live fleet and served nothing: inference
/// never picks the control account, so `research` could not route, while
/// `tcr group ls` reported a healthy one-member group.
const CONTROL_ONLY_GROUP: &str = r#"{
      "proxy": { "port": 3456 },
      "controlAccount": "gil@example.com",
      "accounts": [
        { "name": "gil@example.com", "type": "oauth", "accessToken": "at-g",
          "expiresAt": 1893456000000, "priority": 0, "groups": ["research"] },
        { "name": "worker@example.com", "type": "oauth", "accessToken": "at-w",
          "expiresAt": 1893456000000, "priority": 1, "groups": ["dev"] },
        { "name": "worker2@example.com", "type": "oauth", "accessToken": "at-w2",
          "expiresAt": 1893456000000, "priority": 2, "groups": ["dev"] }
      ]
    }"#;

fn group_line(text: &str, group: &str) -> String {
    text.lines()
        .find(|l| {
            l.starts_with(&format!("group {group} ")) || l.starts_with(&format!("group {group}"))
        })
        .unwrap_or_else(|| panic!("no line for group {group} in:\n{text}"))
        .to_string()
}

/// `ls` must distinguish a group that cannot route from one that can — the
/// whole point of the field. `dev` (two ordinary accounts) routes; `research`
/// (control account only) does not, and says why.
#[test]
fn ls_marks_a_control_only_group_as_not_routing() {
    let path = write_config("routes-control-only", CONTROL_ONLY_GROUP);
    let config = load(&path);
    let research = group_line(&render_groups_text(&config), "research");
    assert!(
        research.contains("routes=no") && research.contains("route_block=control-account-only"),
        "a control-account-only group must say it routes nothing: {research}"
    );
    let dev = group_line(&render_groups_text(&config), "dev");
    assert!(
        dev.contains("routes=yes") && !dev.contains("route_block="),
        "an ordinary group must not be flagged: {dev}"
    );
    fs::remove_file(&path).ok();
}

/// The remedy the warning tells an operator to apply must actually clear the
/// flag — otherwise the message sends them somewhere that does not help.
#[test]
fn a_second_member_makes_the_group_route_again() {
    let path = write_config("routes-second-member", CONTROL_ONLY_GROUP);
    add_to_group(&path, "research", "worker@example.com").unwrap();
    let config = load(&path);
    let research = group_line(&render_groups_text(&config), "research");
    assert!(
        research.contains("routes=yes") && !research.contains("route_block="),
        "adding a non-control account must make the group routable: {research}"
    );
    fs::remove_file(&path).ok();
}

/// The row for group `name` in `tcr group ls --json` over `config`.
fn group_row(config: &Config, name: &str) -> serde_json::Value {
    let value: serde_json::Value =
        serde_json::from_str(&render_groups_json(config).unwrap()).unwrap();
    value["groups"]
        .as_array()
        .expect("groups array")
        .iter()
        .find(|r| r["group"] == name)
        .expect("group row")
        .clone()
}

/// Same fact on the JSON surface, which the panel and scripts read.
#[test]
fn ls_json_carries_routes_and_route_block() {
    let path = write_config("routes-json", CONTROL_ONLY_GROUP);
    let config = load(&path);
    let row = |name: &str| group_row(&config, name);
    assert_eq!(row("research")["routes"], serde_json::json!(false));
    assert_eq!(
        row("research")["routeBlock"],
        serde_json::json!(ROUTE_BLOCK_CONTROL_ONLY),
        "the JSON reason must be the same token the text surface prints"
    );
    assert_eq!(row("dev")["routes"], serde_json::json!(true));
    assert_eq!(row("dev")["routeBlock"], serde_json::Value::Null);
    fs::remove_file(&path).ok();
}

/// The warning `group add` prints. It fires only for the control account,
/// and it names both remedies — otherwise it is just an alarm with no exit.
#[test]
fn labelling_the_control_account_warns_and_names_a_remedy() {
    let path = write_config("routes-notice", CONTROL_ONLY_GROUP);
    let config = load(&path);
    let notice = control_account_group_notice(&config, "gil@example.com", "research")
        .expect("labelling the control account must produce a notice");
    assert!(notice.contains("control account"), "{notice}");
    assert!(
        notice.contains("research"),
        "the notice names the group: {notice}"
    );
    assert!(
        notice.contains("tcr control --clear"),
        "the notice names a runnable remedy: {notice}"
    );
    assert!(
        control_account_group_notice(&config, "worker@example.com", "dev").is_none(),
        "an ordinary account must not be warned about"
    );
    fs::remove_file(&path).ok();
}

/// `tcr group add` on a Codex account says on stderr what the label does and
/// does not do, and still writes it: the label parks the account, and the
/// command's idempotent exit-0 contract with TcrBar is unchanged.
#[tokio::test]
async fn group_add_on_a_codex_row_warns_on_stderr() {
    let path = alice_and_codex_bob("group-add-codex", serde_json::json!({})).await;
    let config = load(&path);
    assert_eq!(
            codex_group_add_notice(&config.accounts[1], "orchard"),
            Some("warning: 'bob@example.com/codex' is a Codex account. A Codex session cannot ask for a group, so 'orchard' labels it and can park it, but never routes to it; while 'orchard' is reserved, it serves nothing.".to_string())
        );
    assert_eq!(
        codex_group_add_notice(&config.accounts[0], "orchard"),
        None,
        "a Claude row is an ordinary add"
    );

    add_to_group(&path, "orchard", "bob@example.com/codex")
        .expect("the label is written all the same");
    assert!(load(&path).accounts[1].in_group("orchard"));
    fs::remove_file(&path).ok();
}

/// `tcr group ls` counts a group's Codex members apart, and `routes=` keeps
/// meaning "a Claude session asking for this group can be served": it counts
/// Claude members only, because no Codex request carries a group. A group of
/// Codex members alone says why it routes nothing, and a group with no Codex
/// member reads exactly as it always did.
#[test]
fn group_ls_counts_codex_members_and_routes_counts_anthropic_only() {
    let path = write_config(
        "group-ls-codex",
        r#"{
              "accounts": [
                { "name": "alice@example.com", "type": "oauth", "accessToken": "at-a",
                  "groups": ["mixed", "solo"] },
                { "name": "bob@example.com/codex", "provider": "codex", "type": "oauth",
                  "accessToken": "at-b", "credentialVersion": 2, "groups": ["mixed", "lab"] }
              ]
            }"#,
    );
    let config = load(&path);

    let text = render_groups_text(&config);
    let line = |group: &str| -> String {
        text.lines()
            .find(|line| line.starts_with(&format!("group {group} ")))
            .unwrap_or_else(|| panic!("a line for {group}: {text}"))
            .to_string()
    };
    let mixed = line("mixed");
    assert!(
        mixed.contains(" members=alice@example.com,bob@example.com/codex codex_members=1 "),
        "{mixed}"
    );
    assert!(mixed.contains(" routes=yes "), "{mixed}");
    let lab = line("lab");
    assert!(lab.contains(" codex_members=1 "), "{lab}");
    assert!(lab.contains(" routes=no route_block=codex-only "), "{lab}");
    let solo = line("solo");
    assert!(!solo.contains("codex_members"), "{solo}");
    assert!(solo.contains(" routes=yes "), "{solo}");

    let row = |name: &str| group_row(&config, name);
    assert_eq!(row("mixed")["codexMembers"], serde_json::json!(1));
    assert_eq!(row("mixed")["routes"], serde_json::json!(true));
    assert_eq!(row("mixed")["routeBlock"], serde_json::Value::Null);
    assert_eq!(row("lab")["codexMembers"], serde_json::json!(1));
    assert_eq!(row("lab")["routes"], serde_json::json!(false));
    assert_eq!(row("lab")["routeBlock"], serde_json::json!("codex-only"));
    assert!(
        row("solo").get("codexMembers").is_none(),
        "a group with no Codex member carries no new key: {}",
        row("solo")
    );
    fs::remove_file(&path).ok();
}

// --- group color --------------------------------------------------------

/// A configured color round-trips through config and onto both `ls`
/// renderers, tagged `color_source=set`/`colorSource: "set"`.
#[test]
fn group_color_set_round_trips_through_config_and_ls() {
    let path = write_config("color-set", GROUPED_ACCOUNTS);
    set_group_color(&path, "codereview", Some("#32D74B")).unwrap();
    let config = load(&path);
    assert_eq!(
        config.group_color("codereview"),
        ("#32d74b".to_string(), config::ColorSource::Set),
        "hex normalizes to lowercase on write"
    );
    let text = render_groups_text(&config);
    let line = text
        .lines()
        .find(|l| l.starts_with("group codereview"))
        .expect("codereview line");
    assert!(
        line.contains("color=#32d74b") && line.contains("color_source=set"),
        "text ls carries the set color: {line}"
    );
    let json_value: serde_json::Value =
        serde_json::from_str(&render_groups_json(&config).unwrap()).unwrap();
    let row = json_value["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["group"] == "codereview")
        .expect("codereview row");
    assert_eq!(row["color"], serde_json::json!("#32d74b"));
    assert_eq!(row["colorSource"], serde_json::json!("set"));
    fs::remove_file(&path).ok();
}

/// THE test for problem 3's trigger condition: a known low-contrast pick
/// (`#d69451`, `config::best_readable_apca` measures ~54.1, below the 60
/// floor) produces a warning naming the measured value and a derived
/// alternative that itself clears the floor — driving
/// `low_contrast_color_warning`'s actual branch, not just checking it
/// compiles.
#[test]
fn low_contrast_color_warning_fires_and_names_a_good_alternative() {
    let low_contrast = "#d69451";
    assert!(
        config::best_readable_apca(low_contrast) < 60.0,
        "test fixture assumption broken: {low_contrast} must score below the APCA floor"
    );
    let warning = low_contrast_color_warning(low_contrast, "codereview").expect("must warn");
    assert!(warning.contains(low_contrast), "names the color: {warning}");
    assert!(warning.contains("codereview"), "names the group: {warning}");
    let alternative = config::derive_group_color("codereview");
    assert!(
        warning.contains(&alternative),
        "names a derived alternative ({alternative}): {warning}"
    );
    assert!(
        config::best_readable_apca(&alternative) >= 60.0,
        "the alternative named in the warning must itself be readable"
    );
}

/// Control for the test above: a color that DOES clear the APCA floor
/// gets no warning at all — the low-contrast test is exercising the
/// warning branch specifically, not a function that always returns `Some`.
#[test]
fn low_contrast_color_warning_is_silent_for_a_readable_pick() {
    let readable = "#32d74b";
    assert!(
        config::best_readable_apca(readable) >= 60.0,
        "test fixture assumption broken: {readable} must clear the APCA floor"
    );
    assert_eq!(low_contrast_color_warning(readable, "codereview"), None);
}

/// `set_group_color` itself must still SUCCEED (never refuse) and
/// actually persist a low-contrast pick to disk — the warning is
/// advisory, not a gate. Exercises the real CLI entry point end to end,
/// distinct from the two pure-function tests above.
#[test]
fn set_group_color_accepts_a_low_contrast_pick_instead_of_refusing() {
    let low_contrast = "#d69451";
    let path = write_config("color-low-contrast", GROUPED_ACCOUNTS);
    let result = set_group_color(&path, "codereview", Some(low_contrast));
    assert!(
        result.is_ok(),
        "a low-contrast color must warn, not be refused: {result:?}"
    );
    let config = load(&path);
    assert_eq!(
        config.group_color("codereview"),
        (low_contrast.to_string(), config::ColorSource::Set),
        "the low-contrast color must actually be set, not silently dropped"
    );
    fs::remove_file(&path).ok();
}

/// An unconfigured group gets a derived color that is stable across
/// independent reads and differs from another name's — the bridge's
/// "stable across runs, differs by name" requirement, checked directly
/// against the config layer rather than the CLI (no write happens here).
#[test]
fn unconfigured_group_color_is_derived_stable_and_distinguishes_names() {
    let path = write_config("color-derived", GROUPED_ACCOUNTS);
    let config = load(&path);
    let (first, source) = config.group_color("codereview");
    assert_eq!(source, config::ColorSource::Derived);
    let (second, _) = config.group_color("codereview");
    assert_eq!(first, second, "same name, same process, same color");
    // Independent load — a fresh process reading the same file — must
    // derive the identical color: stability cannot depend on anything
    // but the group name.
    let reloaded = load(&path);
    let (third, _) = reloaded.group_color("codereview");
    assert_eq!(first, third, "stable across independent loads");

    let (dev_color, _) = config.group_color("dev");
    assert_ne!(
        first, dev_color,
        "two different group names must not collide onto the same color"
    );
    fs::remove_file(&path).ok();
}

/// `--clear` reverts a configured color to the derived default — the
/// SAME derived value `unconfigured_group_color_is_derived_...` above
/// pins, not a different fallback.
#[test]
fn group_color_clear_reverts_to_derived() {
    let path = write_config("color-clear", GROUPED_ACCOUNTS);
    let (derived_before, _) = load(&path).group_color("codereview");

    set_group_color(&path, "codereview", Some("#123456")).unwrap();
    assert_eq!(
        load(&path).group_color("codereview"),
        ("#123456".to_string(), config::ColorSource::Set)
    );

    set_group_color(&path, "codereview", None).unwrap();
    let (after_clear, source) = load(&path).group_color("codereview");
    assert_eq!(source, config::ColorSource::Derived);
    assert_eq!(
        after_clear, derived_before,
        "clearing must land on exactly the value that was always derived, \
             not some other fallback"
    );
    fs::remove_file(&path).ok();
}

/// Strictly invalid hex is rejected, and — like `reserve`'s safety
/// rail — the config is byte-identical after the refusal: a rejected
/// write must never touch the file.
#[test]
fn group_color_rejects_invalid_hex_and_leaves_file_untouched() {
    let path = write_config("color-bad-hex", GROUPED_ACCOUNTS);
    let before = fs::read_to_string(&path).unwrap();
    for bad in ["32d74b", "#32d74", "#zzzzzz", "#12345g", ""] {
        let result = set_group_color(&path, "codereview", Some(bad));
        assert!(result.is_err(), "{bad:?} must be rejected");
    }
    let after = fs::read_to_string(&path).unwrap();
    assert_eq!(before, after, "a rejected hex must not touch the file");
    fs::remove_file(&path).ok();
}

/// `#RGB` shorthand expands to the doubled `#RRGGBB` form.
#[test]
fn group_color_accepts_short_hex_form() {
    let path = write_config("color-short-hex", GROUPED_ACCOUNTS);
    set_group_color(&path, "codereview", Some("#abc")).unwrap();
    assert_eq!(
        load(&path).group_color("codereview"),
        ("#aabbcc".to_string(), config::ColorSource::Set)
    );
    fs::remove_file(&path).ok();
}

/// Setting a color on a group no account carries is an error listing the
/// configured groups — matching `add_to_group`'s unknown-account message
/// shape (via `find_account_by_name`).
#[test]
fn group_color_on_unknown_group_lists_configured_groups() {
    let path = write_config("color-unknown-group", GROUPED_ACCOUNTS);
    let before = fs::read_to_string(&path).unwrap();
    let err = set_group_color(&path, "typo-group", Some("#32d74b"))
        .expect_err("an unconfigured group name must be refused");
    let msg = err.to_string();
    assert!(msg.contains("codereview") && msg.contains("dev"), "{msg}");
    let after = fs::read_to_string(&path).unwrap();
    assert_eq!(
        before, after,
        "a refused group name must not touch the file"
    );
    fs::remove_file(&path).ok();
}

#[test]
fn group_ls_json_lists_an_account_in_two_groups_under_both() {
    let config = config::load(&write_config("group-ls-json", GROUPED_ACCOUNTS)).unwrap();
    let rendered = render_groups_json(&config).unwrap();
    let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
    let groups = value["groups"].as_array().unwrap();
    let codereview = groups
        .iter()
        .find(|g| g["group"] == "codereview")
        .expect("codereview group must be present");
    let members: Vec<&str> = codereview["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m.as_str().unwrap())
        .collect();
    assert!(
        members.contains(&"alice@example.com"),
        "alice is in codereview AND dev — she must appear under codereview too: {members:?}"
    );
    assert!(members.contains(&"bob@example.com"));
    let dev = groups
        .iter()
        .find(|g| g["group"] == "dev")
        .expect("dev group must be present");
    let dev_members: Vec<&str> = dev["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m.as_str().unwrap())
        .collect();
    assert!(
        dev_members.contains(&"alice@example.com"),
        "alice must ALSO appear under dev: {dev_members:?}"
    );
    assert_eq!(value["ungrouped"], serde_json::json!(1)); // carol only
}

#[test]
fn group_ls_text_reports_ungrouped_count() {
    let config = config::load(&write_config("group-ls-text", GROUPED_ACCOUNTS)).unwrap();
    let rendered = render_groups_text(&config);
    assert!(rendered.contains("ungrouped accounts=1"), "{rendered}");
    assert!(rendered.contains("group codereview"), "{rendered}");
    assert!(rendered.contains("group dev"), "{rendered}");
}

// --- enable / disable --------------------------------------------------
