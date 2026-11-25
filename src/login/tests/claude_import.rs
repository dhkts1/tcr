use super::*;

// --- the Claude Code import ----------------------------------------------

/// A fake Claude Code credentials document, values invented.
const FAKE_CLAUDE_CODE_JSON: &str = r#"{
      "claudeAiOauth": {
        "accessToken": "at-imported-FAKE",
        "refreshToken": "rt-imported-FAKE",
        "expiresAt": 1789000000000,
        "subscriptionType": "max"
      }
    }"#;

/// The whole point of importing this credential rather than a
/// `setup-token` one: it carries a refresh token and a real expiry, so the
/// row lands able to refresh itself. Mutation note: drop `refresh_token`
/// in `tokens_from_claude_code_login` (the shape `login_with_token` has to
/// use) and the `has_refresh` assertion fails; stamp
/// `SETUP_TOKEN_ASSUMED_LIFETIME_MS` instead of the document's own
/// `expiresAt` and the expiry assertion fails.
#[tokio::test]
async fn an_imported_claude_code_login_lands_with_its_refresh_token_and_expiry() {
    let path = std::env::temp_dir().join(format!(
        "tcr-oauth-claude-code-import-{}.json",
        std::process::id()
    ));
    std::fs::write(&path, r#"{ "accounts": [] }"#).unwrap();
    let config = load_or_default(&path).unwrap();

    let login = crate::claude_code_login::parse(FAKE_CLAUDE_CODE_JSON)
        .expect("the fake document parses")
        .expect("it carries a login");
    let tokens = tokens_from_claude_code_login(login);

    let name = finish_login(
        &path,
        WriteRoute::File,
        &config,
        "imported@example.com",
        &tokens,
        profile_named("imported@example.com"),
    )
    .await
    .unwrap();
    assert_eq!(name, "imported@example.com");

    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let row = &doc["accounts"][0];
    assert_eq!(row["name"], serde_json::json!("imported@example.com"));
    assert!(
        row["refreshToken"].is_string(),
        "an imported Claude Code login must land WITH its refresh token"
    );
    assert_eq!(
        row["expiresAt"],
        serde_json::json!(1_789_000_000_000i64),
        "the row's expiry must be the document's own `expiresAt`, in ms"
    );
    std::fs::remove_file(&path).ok();
}

/// A credential with no `expiresAt` is stamped as already expired, so the
/// first use refreshes it — never the year-long `--token` assumption,
/// which would leave a dead credential looking healthy for months.
#[test]
fn a_claude_code_login_with_no_expiry_is_stamped_as_due_a_refresh() {
    let login = crate::claude_code_login::parse(
        r#"{"claudeAiOauth": {"accessToken": "at-FAKE", "refreshToken": "rt-FAKE"}}"#,
    )
    .expect("parses")
    .expect("has a login");
    let before = crate::now_ms();
    let tokens = tokens_from_claude_code_login(login);
    assert!(
        tokens.expires_at_ms >= before && tokens.expires_at_ms <= crate::now_ms(),
        "an unknown expiry must stamp NOW (got {}), not a year out",
        tokens.expires_at_ms
    );
}

/// Seconds-vs-milliseconds is the classic way an imported expiry lands
/// wrong; the same `normalize_expires_at` every other path uses guards it.
#[test]
fn a_seconds_expiry_is_normalized_to_milliseconds() {
    let login = crate::claude_code_login::parse(
        r#"{"claudeAiOauth": {"accessToken": "at-FAKE", "expiresAt": 1789000000}}"#,
    )
    .expect("parses")
    .expect("has a login");
    assert_eq!(
        tokens_from_claude_code_login(login).expires_at_ms,
        1_789_000_000_000
    );
}

/// The plan is shown back as confirmation of WHICH login was taken, and an
/// absent one must not render as `()` or `(None)`.
#[test]
fn the_plan_suffix_is_empty_when_there_is_no_plan() {
    assert_eq!(plan_suffix(Some("max")), " (max)");
    assert_eq!(plan_suffix(None), "");
    assert_eq!(plan_suffix(Some("  ")), "");
}

/// The consequence is stated, not hidden — and stated once, from one
/// constant, so the explicit verb and the auto-import cannot drift.
#[test]
fn the_import_consequence_names_the_re_login_cost() {
    assert!(
        CLAUDE_CODE_IMPORT_CONSEQUENCE.contains("log in again")
            && CLAUDE_CODE_IMPORT_CONSEQUENCE.contains("single-use"),
        "the user must be told the `claude` CLI will need one browser login: \
         {CLAUDE_CODE_IMPORT_CONSEQUENCE}"
    );
}
