use super::*;

// --- `--account` identity assertion (`tcr login --account`) ------------

/// One person (`uuid-person`) in two orgs, Corp and Personal, one row each.
const ONE_PERSON_IN_TWO_ORGS: &str = r#"{ "accounts": [
            { "name": "me@example.com (Corp)", "type": "oauth", "accessToken": "at-corp",
              "refreshToken": "rt-corp-DEAD", "expiresAt": 1893456000000, "priority": 0,
              "accountUuid": "uuid-person", "orgUuid": "org-corp", "orgName": "Corp" },
            { "name": "me@example.com (Personal)", "type": "oauth", "accessToken": "at-pers",
              "refreshToken": "rt-personal-ALIVE", "expiresAt": 1893456000000, "priority": 1,
              "accountUuid": "uuid-person", "orgUuid": "org-personal", "orgName": "Personal" }
        ] }"#;

/// A mismatch between the requested account and the identity that came
/// back writes NOTHING — the config file is byte-identical afterwards —
/// and the error names both sides.
#[tokio::test]
async fn finish_login_checked_mismatch_writes_nothing() {
    let path = login_file("account-mismatch", &two_account_seed());
    let config = load_or_default(&path).unwrap();
    let before = std::fs::read_to_string(&path).unwrap();

    let err = finish_login_checked(
        &path,
        WriteRoute::File,
        &config,
        "alice@example.com",
        &tokens("at-new", "rt-new", 1_893_456_000_000),
        profile_named("mallory@example.com"),
        Some("alice@example.com"),
    )
    .await
    .unwrap_err();

    let msg = err.to_string();
    assert!(msg.contains("alice@example.com"), "names requested: {msg}");
    assert!(msg.contains("mallory@example.com"), "names returned: {msg}");

    let after = std::fs::read_to_string(&path).unwrap();
    assert_eq!(before, after, "a mismatch must leave the config untouched");

    std::fs::remove_file(&path).ok();
}

/// A match writes normally, through the exact same `finish_login` path as
/// today's no-`--account` flow.
#[tokio::test]
async fn finish_login_checked_match_writes_normally() {
    let path = login_file("account-match", &two_account_seed());
    let config = load_or_default(&path).unwrap();

    let name = finish_login_checked(
        &path,
        WriteRoute::File,
        &config,
        "alice@example.com",
        &tokens("at-fresh", "rt-fresh", 1_893_456_000_000),
        profile_named("alice@example.com"),
        Some("alice@example.com"),
    )
    .await
    .unwrap();
    assert_eq!(name, "alice@example.com");

    let after = std::fs::read_to_string(&path).unwrap();
    assert!(
        after.contains("rt-fresh"),
        "a matching identity must be written: {after}"
    );

    std::fs::remove_file(&path).ok();
}

/// `fetch_profile`'s all-`None` failure return (`fetch_profile_at`, `src/oauth/profile.rs`)
/// must refuse rather than pass by default — an assertion that cannot be
/// evaluated is not evidence of a match.
#[tokio::test]
async fn finish_login_checked_all_none_profile_refuses() {
    let path = login_file("account-allnone", &two_account_seed());
    let config = load_or_default(&path).unwrap();
    let before = std::fs::read_to_string(&path).unwrap();

    let all_none = Profile {
        email: None,
        account_uuid: None,
        org_uuid: None,
        org_name: None,
        organization_type: None,
        rate_limit_tier: None,
        seat_tier: None,
        plan_display_name: None,
    };
    let err = finish_login_checked(
        &path,
        WriteRoute::File,
        &config,
        "alice@example.com",
        &tokens("at-new", "rt-new", 1_893_456_000_000),
        all_none,
        Some("alice@example.com"),
    )
    .await
    .unwrap_err();
    assert!(
        err.to_string().contains("alice@example.com"),
        "{}",
        err.to_string()
    );

    let after = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        before, after,
        "an unconfirmable identity must write nothing"
    );

    std::fs::remove_file(&path).ok();
}

/// An `--account` naming no row is an error, not a guess — the same
/// zero/one/many rule `crate::config::Config::resolve_account` enforces for every other
/// command, and the arm that survives now that a name can never match two
/// rows. The query used here is the email PORTION of a migrated row's name,
/// which is precisely what used to match both of them.
#[tokio::test]
async fn finish_login_checked_unmatched_query_is_an_error() {
    let seed = r#"{ "accounts": [
            { "name": "dup@example.com", "type": "oauth", "accessToken": "at-1",
              "refreshToken": "rt-1", "expiresAt": 1893456000000, "priority": 0,
              "orgUuid": "org-a", "orgName": "Corp A" },
            { "name": "dup@example.com", "type": "oauth", "accessToken": "at-2",
              "refreshToken": "rt-2", "expiresAt": 1893456000000, "priority": 1,
              "orgUuid": "org-b", "orgName": "Corp B" }
        ] }"#;
    let path = login_file("account-ambiguous", seed);
    // Migrates: the two rows become `dup@example.com` and
    // `dup@example.com/corp-b`. `corp-b` on its own names neither.
    let config = load_or_default(&path).unwrap();
    assert_eq!(config.accounts[1].name, "dup@example.com/corp-b");
    let before = std::fs::read_to_string(&path).unwrap();

    let err = finish_login_checked(
        &path,
        WriteRoute::File,
        &config,
        "dup@example.com",
        &tokens("at-new", "rt-new", 1_893_456_000_000),
        profile_named("dup@example.com"),
        Some("corp-b"),
    )
    .await
    .unwrap_err();
    assert!(
        err.to_string().contains("no account matches 'corp-b'"),
        "{}",
        err.to_string()
    );

    let after = std::fs::read_to_string(&path).unwrap();
    assert_eq!(before, after, "an unmatched query must not guess and write");

    std::fs::remove_file(&path).ok();
}

/// The migration is what makes a re-login of one org addressable at all:
/// the loader gives each row its own name, and `--account <that name>`
/// resolves to exactly one row and writes only that row's credential.
///
/// This is the same fleet shape that used to be an ambiguous refusal, then
/// a refusal plus an org flag. Note the assertion is on which row got
/// `rt-new`: a re-login that guessed would still return a plausible name.
#[tokio::test]
async fn finish_login_checked_re_logs_in_the_named_row_of_a_migrated_duplicate() {
    let seed = r#"{ "accounts": [
            { "name": "dup@example.com", "type": "oauth", "accessToken": "at-1",
              "refreshToken": "rt-1", "expiresAt": 1893456000000, "priority": 0,
              "accountUuid": "uuid-dup", "orgUuid": "org-a", "orgName": "Corp A" },
            { "name": "dup@example.com", "type": "oauth", "accessToken": "at-2",
              "refreshToken": "rt-2", "expiresAt": 1893456000000, "priority": 1,
              "accountUuid": "uuid-dup", "orgUuid": "org-b", "orgName": "Corp B" }
        ] }"#;
    let path = login_file("account-org-disambig", seed);
    // Loading migrates the duplicate: Corp A keeps the bare email (lower
    // priority number), Corp B takes its org's slug.
    let config = load_or_default(&path).unwrap();
    assert_eq!(config.accounts[0].name, "dup@example.com");
    assert_eq!(config.accounts[1].name, "dup@example.com/corp-b");

    let name = finish_login_checked(
        &path,
        WriteRoute::File,
        &config,
        "dup@example.com",
        &tokens("at-new", "rt-new", 1_893_456_000_000),
        profile_with_identity(
            "dup@example.com",
            Some("uuid-dup"),
            Some("org-a"),
            Some("Corp A"),
        ),
        Some("dup@example.com"),
    )
    .await
    .unwrap();
    assert_eq!(name, "dup@example.com");

    let after: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let corp_a = after["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["orgUuid"] == "org-a")
        .expect("Corp A row still present");
    assert_eq!(
        corp_a["refreshToken"],
        serde_json::json!("rt-new"),
        "the disambiguated row must be the one that was written: {after}"
    );

    std::fs::remove_file(&path).ok();
}

/// THE CANONICAL FAILURE the review found: `account_uuid` alone cannot
/// distinguish a person's two orgs. Config holds the SAME person
/// (`uuid-person`) in both a Corp row and a Personal row, each with its
/// own org and its own refresh token. Corp's token is dead; the operator
/// requests `--account` re-login for Corp. The browser — plausibly, this
/// IS the feature's premise — is signed into Personal instead.
/// `fetch_profile` returns `{uuid: uuid-person, org: org-personal}`. A
/// uuid-only comparison takes the `(Some, Some)` arm and passes, because
/// both rows share the same person. The org-aware check must refuse, and
/// Corp's own refresh token must be UNCHANGED afterwards — not merely
/// "an error came back", which a check that still wrote to the wrong row
/// before erroring would also produce.
#[tokio::test]
async fn finish_login_checked_same_uuid_different_org_is_a_mismatch() {
    let path = login_file("account-same-uuid-diff-org", ONE_PERSON_IN_TWO_ORGS);
    let config = load_or_default(&path).unwrap();
    let before = std::fs::read_to_string(&path).unwrap();

    // The browser authenticated as the SAME person, but the Personal org.
    let wrong_org_profile = profile_with_identity(
        "me@example.com",
        Some("uuid-person"),
        Some("org-personal"),
        Some("Personal"),
    );

    let err = finish_login_checked(
        &path,
        WriteRoute::File,
        &config,
        "me@example.com",
        &tokens("at-new", "rt-new", 1_893_456_000_000),
        wrong_org_profile,
        Some("me@example.com (Corp)"),
    )
    .await
    .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("me@example.com (Corp)"),
        "names requested: {msg}"
    );
    assert!(
        msg.contains("Personal") || msg.contains("me@example.com (Personal)"),
        "names the org actually authenticated: {msg}"
    );

    let after = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        before, after,
        "a same-person-different-org mismatch must leave BOTH rows untouched \
         (Corp's dead token in particular must not be silently 'fixed' onto Personal's row)"
    );

    std::fs::remove_file(&path).ok();
}

/// A fleet of one Claude row and one Codex row, `bob@example.com/codex`, whose proxy is on
/// a port nothing listens on, written to a fresh tempdir.
async fn claude_and_codex_rows() -> (tempfile::TempDir, std::path::PathBuf) {
    let port = DEAD_PORT;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(crate::paths::CONFIG_FILE);
    std::fs::write(
        &path,
        format!(
            r#"{{ "proxy": {{ "port": {port} }}, "accounts": [
                    {{ "name": "alice@example.com", "accessToken": "at-alice" }},
                    {{ "name": "bob@example.com/codex", "provider": "codex",
                       "accessToken": "at-bob" }}
                ] }}"#
        ),
    )
    .unwrap();
    (dir, path)
}

/// `tcr login --account` on a Codex row is refused before the browser sign-in starts and
/// before any event, naming the row and the two commands that can sign it in, with the code
/// a GUI titles it by, and nothing is written. The sign-in is Anthropic's, and the identity
/// it returns can never confirm a Codex row, so going on ended in "sign out and retry" with
/// the same command, every time.
///
/// Watched red: without the refusal, or with it moved below `authorize_and_wait`, `login`
/// starts the sign-in and waits for its callback until the timeout below fires.
#[tokio::test]
async fn a_relogin_of_a_codex_row_is_refused_before_the_browser_opens() {
    let (_dir, path) = claude_and_codex_rows().await;
    let before = std::fs::read(&path).unwrap();

    let err = tokio::time::timeout(
        Duration::from_secs(5),
        login(
            &path,
            false,
            Some("bob@example.com/codex"),
            None,
            LoginUi::Machine,
        ),
    )
    .await
    .expect("refused before the sign-in, which would wait for its callback")
    .expect_err("`tcr login` cannot sign in a Codex row");
    let refusal = err
        .downcast_ref::<LoginRefusal>()
        .expect("a refusal a GUI can title by its code");
    assert_eq!(refusal.code, "account-is-codex");
    assert_eq!(
        refusal.reason,
        "'bob@example.com/codex' is a Codex account; `tcr login` signs in Claude accounts \
         only. Sign it in again with `tcr login --codex --account bob@example.com/codex`, or \
         re-import it with `tcr login --from-codex --account bob@example.com/codex`. Nothing \
         was written."
    );
    assert_eq!(err.to_string(), refusal.reason);
    assert_eq!(std::fs::read(&path).unwrap(), before, "nothing was written");
}

/// `tcr login --codex --account` resolves its row first: a Codex row is the target, a Claude
/// row is refused with the code a GUI titles it by, before codex runs, and a name no row has
/// is the error every `--account` gives it.
///
/// Watched red: without the provider check, the Claude row resolves to `Ok` with that row.
#[tokio::test]
async fn login_codex_account_on_a_claude_row_is_refused_before_any_event() {
    let (_dir, path) = claude_and_codex_rows().await;
    let config = load_or_default(&path).unwrap();

    let err = codex_relogin_target(&config, "alice@example.com")
        .expect_err("a Claude row is never re-signed by a ChatGPT login");
    let refusal = err.downcast_ref::<LoginRefusal>().expect("a coded refusal");
    assert_eq!(refusal.code, "account-is-claude");
    assert_eq!(
        refusal.reason,
        "'alice@example.com' is a Claude account; `tcr login --codex` signs in Codex \
         accounts only. Sign it in again with `tcr login --account alice@example.com`. \
         Nothing was written."
    );
    assert_eq!(
        codex_relogin_target(&config, "bob@example.com/codex")
            .unwrap()
            .name,
        "bob@example.com/codex"
    );
    assert_eq!(
        codex_relogin_target(&config, "nobody@example.com")
            .unwrap_err()
            .to_string(),
        "no account matches 'nobody@example.com'"
    );
}

/// The identity check refuses a Codex row by name too, for `--from-claude-code --account`,
/// which reaches it with a credential already in hand: a Codex row is not an account whose
/// browser signed in as someone else.
///
/// Watched red: without the refusal, the error is the mismatch, "requested re-login for …,
/// but the browser authenticated as …".
#[tokio::test]
async fn the_identity_check_refuses_a_codex_row_by_name() {
    let (_dir, path) = claude_and_codex_rows().await;
    let config = load_or_default(&path).unwrap();

    let err = assert_requested_identity(
        &config,
        "bob@example.com/codex",
        &profile_named("bob@example.com"),
    )
    .expect_err("a Codex row is never confirmed by an Anthropic identity");
    let text = err.to_string();
    assert!(
        text.contains("'bob@example.com/codex' is a Codex account")
            && text.contains("`tcr login` signs in Claude accounts only"),
        "{text}"
    );
}

/// The mirror of the canonical failure: the SAME uuid AND the SAME org
/// must still succeed — the org-aware check is not merely stricter, it is
/// exactly as permissive as `upsert_account`'s own write-time resolution.
#[tokio::test]
async fn finish_login_checked_same_uuid_same_org_matches() {
    let path = login_file("account-same-uuid-same-org", ONE_PERSON_IN_TWO_ORGS);
    let config = load_or_default(&path).unwrap();

    let right_profile = profile_with_identity(
        "me@example.com",
        Some("uuid-person"),
        Some("org-corp"),
        Some("Corp"),
    );

    let name = finish_login_checked(
        &path,
        WriteRoute::File,
        &config,
        "me@example.com (Corp)",
        &tokens("at-fresh-corp", "rt-fresh-corp", 1_893_456_000_000),
        right_profile,
        Some("me@example.com (Corp)"),
    )
    .await
    .unwrap();
    assert_eq!(name, "me@example.com (Corp)");

    let after: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let corp = after["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["orgUuid"] == "org-corp")
        .expect("Corp row still present");
    assert_eq!(corp["refreshToken"], serde_json::json!("rt-fresh-corp"));
    let personal = after["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["orgUuid"] == "org-personal")
        .expect("Personal row untouched and still present");
    assert_eq!(
        personal["refreshToken"],
        serde_json::json!("rt-personal-ALIVE"),
        "Personal's own token must be untouched by Corp's re-login"
    );

    std::fs::remove_file(&path).ok();
}

/// A requested account with no stored `account_uuid` AND a non-email
/// display name (e.g. an account named "work") cannot be verified by
/// email-equality either — there is nothing on file to compare the
/// browser identity against. The message must not suggest "sign out",
/// which cannot help here, and it must still refuse (fail closed) rather
/// than accept blindly.
#[tokio::test]
async fn finish_login_checked_non_email_name_with_no_uuid_refuses_honestly() {
    let seed = r#"{ "accounts": [
            { "name": "work", "type": "oauth", "accessToken": "at-work",
              "refreshToken": "rt-work-DEAD", "expiresAt": 1893456000000, "priority": 0 }
        ] }"#;
    let path = login_file("account-non-email-name", seed);
    let config = load_or_default(&path).unwrap();
    let before = std::fs::read_to_string(&path).unwrap();

    let err = finish_login_checked(
        &path,
        WriteRoute::File,
        &config,
        "work",
        &tokens("at-new", "rt-new", 1_893_456_000_000),
        profile_named("someone@example.com"),
        Some("work"),
    )
    .await
    .unwrap_err();

    let msg = err.to_string();
    assert!(
        !msg.to_lowercase().contains("sign out"),
        "advising a sign-out that cannot help is dishonest: {msg}"
    );
    assert!(
        msg.contains("no stored account id") || msg.contains("nothing on file"),
        "must state the real reason verification is impossible: {msg}"
    );

    let after = std::fs::read_to_string(&path).unwrap();
    assert_eq!(before, after, "must still fail closed and write nothing");

    std::fs::remove_file(&path).ok();
}

/// No `--account` behaves exactly as before: no identity check runs at
/// all, so an unrelated returned identity still writes normally (this is
/// the add-a-new-account path, which must not grow an assertion it
/// cannot satisfy).
#[tokio::test]
async fn finish_login_checked_no_account_skips_the_check() {
    let path = login_file("account-none", &two_account_seed());
    let config = load_or_default(&path).unwrap();

    let name = finish_login_checked(
        &path,
        WriteRoute::File,
        &config,
        "carol@example.com",
        &tokens("at-carol", "rt-carol", 1_893_456_000_000),
        profile_named("carol@example.com"),
        None,
    )
    .await
    .unwrap();
    assert_eq!(name, "carol@example.com");

    let after = std::fs::read_to_string(&path).unwrap();
    assert!(after.contains("carol@example.com"), "{after}");

    std::fs::remove_file(&path).ok();
}
