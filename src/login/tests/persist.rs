use super::*;

// --- config append / update --------------------------------------------

/// An identity-only [`Profile`] for the [`upsert_account`] tests — the
/// three fields those tests vary, with no plan and no email (the name is
/// passed separately).
fn ident(account_uuid: Option<&str>, org_uuid: Option<&str>, org_name: Option<&str>) -> Profile {
    Profile {
        email: None,
        account_uuid: account_uuid.map(str::to_string),
        org_uuid: org_uuid.map(str::to_string),
        org_name: org_name.map(str::to_string),
        organization_type: None,
        rate_limit_tier: None,
        seat_tier: None,
        plan_display_name: None,
    }
}

/// A plan is not an identity: an org that upgraded, or an account that
/// joined a Team, must show its NEW plan after a re-login rather than the
/// one it was first seen on. This is the one place `upsert_account` treats
/// a field differently from the uuid/org fields beside it.
#[test]
fn upsert_overwrites_a_stale_plan_while_still_backfilling_identity() {
    let mut config: Config = serde_json::from_str(
        r#"{ "accounts": [
                { "name": "me@example.com", "type": "oauth", "accessToken": "old-at",
                  "orgName": "Kept", "organizationType": "claude_max",
                  "rateLimitTier": "default_claude_max_5x" }
            ] }"#,
    )
    .unwrap();

    let mut profile = ident(None, None, Some("Ignored"));
    profile.organization_type = Some("claude_team".to_string());
    profile.rate_limit_tier = Some("default_raven".to_string());
    profile.seat_tier = Some("team_standard".to_string());
    upsert_account(
        &mut config,
        "me@example.com",
        &tokens("new-at", "new-rt", 999),
        &profile,
    )
    .unwrap();

    let account = &config.accounts[0];
    assert_eq!(
        account.org_name.as_deref(),
        Some("Kept"),
        "identity stays backfill-only — a known org name is never overwritten"
    );
    assert_eq!(account.organization_type.as_deref(), Some("claude_team"));
    assert_eq!(account.rate_limit_tier.as_deref(), Some("default_raven"));
    assert_eq!(account.seat_tier.as_deref(), Some("team_standard"));
}

/// A profile fetch that failed carries no plan, and "I did not learn this"
/// must never blank a plan already on the row.
#[test]
fn upsert_never_blanks_a_known_plan_when_the_profile_carried_none() {
    let mut config: Config = serde_json::from_str(
        r#"{ "accounts": [
                { "name": "me@example.com", "type": "oauth", "accessToken": "old-at",
                  "organizationType": "claude_team", "seatTier": "team_standard" }
            ] }"#,
    )
    .unwrap();

    upsert_account(
        &mut config,
        "me@example.com",
        &tokens("new-at", "new-rt", 999),
        &ident(None, None, None),
    )
    .unwrap();

    assert_eq!(
        config.accounts[0].organization_type.as_deref(),
        Some("claude_team")
    );
    assert_eq!(
        config.accounts[0].seat_tier.as_deref(),
        Some("team_standard")
    );
}

/// A brand-new account carries the plan the login just read.
#[test]
fn upsert_stores_the_plan_on_a_freshly_appended_account() {
    let mut config: Config = serde_json::from_str(r#"{ "accounts": [] }"#).unwrap();
    upsert_account(
        &mut config,
        "new@example.com",
        &tokens("at1", "rt1", 111),
        &profile_with_plan(
            "new@example.com",
            "claude_max",
            Some("default_claude_max_20x"),
            None,
        ),
    )
    .unwrap();

    assert_eq!(
        config.accounts[0].organization_type.as_deref(),
        Some("claude_max")
    );
    assert_eq!(
        config.accounts[0].rate_limit_tier.as_deref(),
        Some("default_claude_max_20x")
    );
    assert_eq!(config.accounts[0].seat_tier, None);
}

#[test]
fn upsert_appends_new_account() {
    let mut config: Config = serde_json::from_str(r#"{ "accounts": [] }"#).unwrap();
    upsert_account(
        &mut config,
        "new@example.com",
        &tokens("at1", "rt1", 111),
        &ident(None, None, None),
    )
    .unwrap();

    assert_eq!(config.accounts.len(), 1);
    let account = &config.accounts[0];
    assert_eq!(account.name, "new@example.com");
    assert_eq!(account.account_type, config::AccountType::OAuth);
    assert_eq!(account.access_token, "at1");
    assert_eq!(account.refresh_token.as_deref(), Some("rt1"));
    assert_eq!(account.expires_at, Some(111));
    assert_eq!(account.priority, Some(0));
}

#[test]
fn upsert_updates_existing_email_without_duplicating() {
    let mut config: Config = serde_json::from_str(
        r#"{ "accounts": [
                { "name": "me@example.com", "type": "oauth", "accessToken": "old-at",
                  "refreshToken": "old-rt", "expiresAt": 100, "priority": 0 }
            ] }"#,
    )
    .unwrap();

    upsert_account(
        &mut config,
        "me@example.com",
        &tokens("new-at", "new-rt", 999),
        &ident(None, None, None),
    )
    .unwrap();

    // Same account count — updated in place, not duplicated.
    assert_eq!(config.accounts.len(), 1);
    let account = &config.accounts[0];
    assert_eq!(account.access_token, "new-at");
    assert_eq!(account.refresh_token.as_deref(), Some("new-rt"));
    assert_eq!(account.expires_at, Some(999));
    // Existing priority is preserved on update.
    assert_eq!(account.priority, Some(0));
}

#[test]
fn upsert_assigns_next_priority_for_additional_account() {
    let mut config: Config = serde_json::from_str(
        r#"{ "accounts": [
                { "name": "first@example.com", "type": "oauth", "accessToken": "at",
                  "refreshToken": "rt", "expiresAt": 100, "priority": 3 }
            ] }"#,
    )
    .unwrap();

    upsert_account(
        &mut config,
        "second@example.com",
        &tokens("at2", "rt2", 200),
        &ident(None, None, None),
    )
    .unwrap();

    assert_eq!(config.accounts.len(), 2);
    // max existing priority (3) + 1.
    assert_eq!(config.accounts[1].priority, Some(4));
}

#[test]
fn upsert_second_org_same_email_appends_distinct_account() {
    // #0: the same email logging into a DIFFERENT org must NOT overwrite the
    // first — once both entries carry org keys they are distinct accounts.
    let mut config: Config = serde_json::from_str(r#"{ "accounts": [] }"#).unwrap();
    upsert_account(
        &mut config,
        "me@example.com",
        &tokens("at-corp", "rt-corp", 100),
        &ident(Some("uuid-person"), Some("org-corp"), Some("Corp")),
    )
    .unwrap();
    upsert_account(
        &mut config,
        "me@example.com",
        &tokens("at-pers", "rt-pers", 200),
        &ident(Some("uuid-person"), Some("org-personal"), Some("Personal")),
    )
    .unwrap();

    assert_eq!(config.accounts.len(), 2, "second org is a distinct account");
    assert_eq!(config.accounts[0].access_token, "at-corp");
    assert_eq!(config.accounts[1].access_token, "at-pers");
    assert_eq!(config.accounts[1].org_uuid.as_deref(), Some("org-personal"));
}

#[test]
fn upsert_backfills_legacy_entry_org_without_duplicating() {
    // A legacy entry (uuid but no org) meeting a freshly-profiled login of the
    // same person backfills the org in place rather than duplicating.
    let mut config: Config = serde_json::from_str(
        r#"{ "accounts": [
                { "name": "me@example.com", "type": "oauth", "accountUuid": "uuid-person",
                  "accessToken": "old-at", "refreshToken": "old-rt", "expiresAt": 100, "priority": 0 }
            ] }"#,
    )
    .unwrap();

    upsert_account(
        &mut config,
        "me@example.com",
        &tokens("new-at", "new-rt", 999),
        &ident(Some("uuid-person"), Some("org-corp"), Some("Corp")),
    )
    .unwrap();

    assert_eq!(
        config.accounts.len(),
        1,
        "legacy entry backfilled, not duplicated"
    );
    let account = &config.accounts[0];
    assert_eq!(account.access_token, "new-at");
    assert_eq!(account.org_uuid.as_deref(), Some("org-corp"));
    assert_eq!(account.org_name.as_deref(), Some("Corp"));
}

/// THE MEASURED FAILURE bridge item B exists to fix: entries
/// `[{u1, org-a, at-ORG-A/rt-ORG-A}, {u1, no org, at-LEGACY/rt-LEGACY}]`
/// with a login identity `{u1, no org}`. Under the old `.find()`
/// first-match-wins, `same_identity` matches BOTH rows — a legacy no-org
/// entry loosely matches every org of that person — so `.find()` silently
/// took index 0, stamping the fresh credential over org-a's row and
/// destroying its single-use refresh token. `identity::resolve` exists
/// exactly to break this tie via `same_identity_strict`: only the legacy
/// row (also org-less) is a STRICT match, so the target resolves to it.
#[test]
fn upsert_resolves_the_legacy_two_org_tie_onto_the_legacy_row() {
    let mut config: Config = serde_json::from_str(
        r#"{ "accounts": [
                { "name": "me@example.com", "type": "oauth", "accountUuid": "u1",
                  "orgUuid": "org-a", "orgName": "Org A",
                  "accessToken": "at-ORG-A", "refreshToken": "rt-ORG-A",
                  "expiresAt": 100, "priority": 0 },
                { "name": "me@example.com", "type": "oauth", "accountUuid": "u1",
                  "accessToken": "at-LEGACY", "refreshToken": "rt-LEGACY",
                  "expiresAt": 100, "priority": 1 }
            ] }"#,
    )
    .unwrap();

    upsert_account(
        &mut config,
        "me@example.com",
        &tokens("at-NEW", "rt-NEW", 999),
        &ident(Some("u1"), None, None),
    )
    .unwrap();

    assert_eq!(config.accounts.len(), 2, "no new row appended");
    assert_eq!(
        config.accounts[0].access_token, "at-ORG-A",
        "org-a's row must be untouched: {:?}",
        config.accounts[0]
    );
    assert_eq!(
        config.accounts[0].refresh_token.as_deref(),
        Some("rt-ORG-A"),
        "org-a's refresh token is single-use — clobbering it bricks the account"
    );
    assert_eq!(
        config.accounts[1].access_token, "at-NEW",
        "the legacy row (also org-less) is the strict match, not org-a"
    );
    assert_eq!(config.accounts[1].refresh_token.as_deref(), Some("rt-NEW"));
}

/// A genuine, unbreakable tie — two rows sharing the exact same identity —
/// refuses rather than guesses, and writes NOTHING: the config must be
/// byte-for-byte unchanged after the refusal.
#[test]
fn upsert_refuses_and_writes_nothing_on_an_unbreakable_tie() {
    let mut config: Config = serde_json::from_str(
        r#"{ "accounts": [
                { "name": "me@example.com", "type": "oauth", "accountUuid": "u1",
                  "orgUuid": "org-a", "orgName": "Org A",
                  "accessToken": "at-1", "refreshToken": "rt-1",
                  "expiresAt": 100, "priority": 0 },
                { "name": "me@example.com", "type": "oauth", "accountUuid": "u1",
                  "orgUuid": "org-a", "orgName": "Org A",
                  "accessToken": "at-2", "refreshToken": "rt-2",
                  "expiresAt": 100, "priority": 1 }
            ] }"#,
    )
    .unwrap();
    let before = serde_json::to_value(&config).unwrap();

    let err = upsert_account(
        &mut config,
        "me@example.com",
        &tokens("at-NEW", "rt-NEW", 999),
        &ident(Some("u1"), Some("org-a"), Some("Org A")),
    )
    .expect_err("an unbreakable tie must refuse rather than guess");
    assert!(err.to_string().contains("ambiguous"), "{err}");

    assert_eq!(
        serde_json::to_value(&config).unwrap(),
        before,
        "nothing was written on refusal"
    );
}

/// THE BITING TEST for the whole-file-write hazard `finish_login` exists to
/// remove. The documented failure: `login()` loads a config snapshot, does
/// slow async work (a profile fetch, an unbounded stdin prompt), and THEN
/// whole-file `config::save`s that now-stale snapshot — silently reverting
/// any account the live server rotated in the meantime. Refresh tokens are
/// single-use, so that does not just lose a write, it bricks the reverted
/// account.
///
/// Reproduced here without a real browser: seed a live server + config file
/// with two accounts, take a CLIENT-SIDE snapshot (what `login()` would be
/// holding across its own async window), then have the LIVE SERVER itself
/// rotate one of those accounts' credentials — via the same
/// `/_tcr/accounts` route, a real re-login — AFTER the snapshot was taken.
/// Only then does `finish_login` run, on the STALE snapshot, adding a THIRD
/// account live. If it whole-file-saved that snapshot, the rotation would
/// be reverted. It must not be.
#[tokio::test]
async fn finish_login_live_path_does_not_clobber_a_concurrent_rotation() {
    let (listener, port) = crate::test_support::loopback::listener().await;
    let path = std::env::temp_dir().join(format!(
        "tcr-oauth-live-clobber-{}-{port}.json",
        std::process::id()
    ));
    let seed = format!(
        r#"{{ "proxy": {{ "port": {port} }}, "accounts": [
                {{ "name": "alice@example.com", "type": "oauth", "accessToken": "at-alice",
                  "refreshToken": "rt-alice", "expiresAt": 1893456000000, "priority": 0 }},
                {{ "name": "bob@example.com", "type": "oauth", "accessToken": "at-bob-STALE",
                  "refreshToken": "rt-bob-STALE", "expiresAt": 1893456000000, "priority": 1 }}
            ] }}"#
    );
    std::fs::write(&path, &seed).unwrap();
    serve_live_proxy(listener, &seed, Some(path.clone()));

    // The stale snapshot: what `login()` would have loaded before its own
    // async window (profile fetch / stdin prompt) opened.
    let client_config = load_or_default(&path).unwrap();

    // The live server rotates bob's credentials WHILE that snapshot is
    // held — a real re-login through the same route this unit adds,
    // exactly the race `login()`'s doc-comment describes.
    let rotated_bob = Account {
        access_token: "at-bob-ROTATED".to_string(),
        refresh_token: Some("rt-bob-ROTATED".to_string()),
        expires_at: Some(1_893_456_000_000),
        priority: None,
        ..Account::fixture("bob@example.com")
    };
    let rotate_config = load_or_default(&path).unwrap();
    crate::live_client::post_add_account(&rotate_config, &rotated_bob)
        .await
        .expect("the live re-login must succeed");

    // Sanity: the rotation really did land on disk before the race window
    // (the `finish_login` call below) closes.
    let after_rotation = std::fs::read_to_string(&path).unwrap();
    assert!(
        after_rotation.contains("rt-bob-ROTATED"),
        "setup: the rotation must be on disk first: {after_rotation}"
    );

    // Now finish a THIRD account's login, live, using the STALE snapshot.
    let name = finish_login(
        &path,
        WriteRoute::Live,
        &client_config,
        "charlie@example.com",
        &tokens("at-charlie", "rt-charlie", 1_893_456_000_000),
        profile_named("charlie@example.com"),
    )
    .await
    .unwrap();
    assert_eq!(name, "charlie@example.com");

    let final_doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let accounts = final_doc["accounts"].as_array().unwrap();
    assert_eq!(
        accounts.len(),
        3,
        "alice, rotated bob, and charlie: {final_doc}"
    );
    let bob = accounts
        .iter()
        .find(|a| a["name"] == "bob@example.com")
        .expect("bob still on disk");
    assert_eq!(
        bob["refreshToken"],
        serde_json::json!("rt-bob-ROTATED"),
        "the live add must NOT whole-file-write a stale snapshot back over \
         bob's rotation: {final_doc}"
    );
    assert!(
        accounts.iter().any(|a| a["name"] == "charlie@example.com"),
        "charlie was added live: {final_doc}"
    );

    std::fs::remove_file(&path).ok();
}

/// A server that answered and REFUSED us (wrong api-key) must surface,
/// never silently fall back to writing the file — that silent fallback is
/// how the two halves end up disagreeing about an account's credentials.
#[tokio::test]
async fn finish_login_live_unauthorized_does_not_fall_back_to_file() {
    let (listener, port) = crate::test_support::loopback::listener().await;
    let seed = format!(r#"{{ "proxy": {{ "port": {port}, "apiKey": "correct-key" }} }}"#);
    let path = std::env::temp_dir().join(format!(
        "tcr-oauth-live-unauthorized-{}-{port}.json",
        std::process::id()
    ));
    std::fs::write(&path, &seed).unwrap();
    serve_live_proxy(listener, &seed, Some(path.clone()));

    // The caller's config carries the WRONG key.
    let client_config: Config = serde_json::from_str(&format!(
        r#"{{ "proxy": {{ "port": {port}, "apiKey": "wrong-key" }} }}"#
    ))
    .unwrap();
    let before = std::fs::read_to_string(&path).unwrap();

    let err = finish_login(
        &path,
        WriteRoute::Live,
        &client_config,
        "dave@example.com",
        &tokens("at-dave", "rt-dave", 1_893_456_000_000),
        profile_named("dave@example.com"),
    )
    .await
    .expect_err("a rejected api-key must not silently write the file");
    assert!(err.to_string().contains("api-key"), "{err}");

    assert_eq!(
        before,
        std::fs::read_to_string(&path).unwrap(),
        "the file must be byte-identical after a surfaced Unauthorized"
    );
    std::fs::remove_file(&path).ok();
}

/// The quiet fallback: nothing is listening at all, so there is no live
/// rotation to disagree with the file — `finish_login` writes it exactly
/// as the historical file-only path always has.
#[tokio::test]
async fn finish_login_no_server_falls_back_to_file_quietly() {
    let dead_port = DEAD_PORT;
    let path = std::env::temp_dir().join(format!(
        "tcr-oauth-live-noserver-{}-{dead_port}.json",
        std::process::id()
    ));
    std::fs::write(
        &path,
        format!(r#"{{ "proxy": {{ "port": {dead_port} }}, "accounts": [] }}"#),
    )
    .unwrap();
    let config = load_or_default(&path).unwrap();

    let name = finish_login(
        &path,
        WriteRoute::Live,
        &config,
        "erin@example.com",
        &tokens("at-erin", "rt-erin", 1_893_456_000_000),
        profile_named("erin@example.com"),
    )
    .await
    .unwrap();
    assert_eq!(name, "erin@example.com");

    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        doc["accounts"][0]["name"],
        serde_json::json!("erin@example.com")
    );
    std::fs::remove_file(&path).ok();
}

/// A temp config file tagged `tag` holding alice on a proxy at `port`, and the stale
/// snapshot of it: what `login()` would have loaded before its own async window (profile
/// fetch / stdin prompt) opened.
fn alice_on_a_proxy_port(tag: &str, port: u16) -> (std::path::PathBuf, Config) {
    let path = std::env::temp_dir().join(format!(
        "tcr-oauth-{tag}-{}-{port}.json",
        std::process::id()
    ));
    let seed = format!(
        r#"{{ "proxy": {{ "port": {port} }}, "accounts": [
                {{ "name": "alice@example.com", "type": "oauth", "accessToken": "at-alice",
                  "refreshToken": "rt-alice", "expiresAt": 1893456000000, "priority": 0 }}
            ] }}"#
    );
    std::fs::write(&path, &seed).unwrap();
    let snapshot = load_or_default(&path).unwrap();
    (path, snapshot)
}

/// Rotate alice's tokens on disk through `config::save_account`, the live server's own
/// surgical persist path, and check the rotation landed.
fn rotate_alice_on_disk(path: &Path) {
    let rotated_alice = Account {
        access_token: "at-alice-ROTATED".to_string(),
        refresh_token: Some("rt-alice-ROTATED".to_string()),
        expires_at: Some(1_893_456_000_000),
        priority: None,
        ..Account::fixture("alice@example.com")
    };
    config::save_account(path, &rotated_alice).expect("the rotation must land on disk");
    let after_rotation = std::fs::read_to_string(path).unwrap();
    assert!(
        after_rotation.contains("rt-alice-ROTATED"),
        "setup: the rotation must be on disk first: {after_rotation}"
    );
}

/// The file at `path` still holds alice's rotated refresh token, so `route` did not
/// whole-file-write a stale snapshot back over `rotation`, and `<added>@example.com`
/// was added.
fn assert_the_rotation_survived_and_one_was_added(
    path: &Path,
    route: &str,
    rotation: &str,
    added: &str,
) {
    let final_doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let accounts = final_doc["accounts"].as_array().unwrap();
    let alice = accounts
        .iter()
        .find(|a| a["name"] == "alice@example.com")
        .expect("alice must still be on disk");
    assert_eq!(
        alice["refreshToken"],
        serde_json::json!("rt-alice-ROTATED"),
        "{route} must NOT whole-file-write a stale snapshot back over {rotation}: {final_doc}"
    );
    assert!(
        accounts
            .iter()
            .any(|a| a["name"] == format!("{added}@example.com").as_str()),
        "{added} was added: {final_doc}"
    );
}

/// THE σ5 EXPERIMENT bridge item A calls for. The WEDGED shape
/// `live_client::probe_incumbent`'s own tests document as measured and real
/// (`a_listener_that_never_answers_is_silent_not_answering`): bound, the
/// connect succeeds off the kernel backlog, and nothing is ever written
/// back — reached on the `Live` route when the capability probe confirmed
/// the route seconds earlier and `post_add_account` then hits its own 5s
/// deadline.
///
/// Reproduces the same race `finish_login_live_path_does_not_clobber_a_concurrent_rotation`
/// does — a stale client-side snapshot plus a write that lands on disk
/// AFTER that snapshot was taken — but via `config::save_account` (the
/// server's own persist primitive) directly, rather than a live HTTP
/// round-trip: the port in THIS scenario is deliberately silent for the
/// test's whole lifetime, so nothing can be routed through it. Before
/// this fix, the `NoAnswer` arm fell through to `persist_via_file`'s
/// WHOLE-FILE `config::save` of that now-stale snapshot, reverting
/// alice's rotation — the exact hazard `finish_login` exists to remove,
/// reintroduced in this one arm (watched failing directly: with that arm
/// reverted to `persist_via_file`, this test fails with `left:
/// "rt-alice" right: "rt-alice-ROTATED"`). Fixed, it lands through
/// `config::save_account`'s surgical single-row write instead, so
/// alice's rotated row survives and the new account is still added.
#[tokio::test]
async fn finish_login_live_timeout_does_not_clobber_other_accounts() {
    // Bound and never accepted: connections queue in the kernel backlog
    // and the connect succeeds, but nothing is ever written back —
    // `post_add_account` times out at its own 5s deadline.
    let port = wedged_port();
    let (path, client_config) = alice_on_a_proxy_port("live-timeout", port);

    // A rotation lands on disk AFTER that snapshot was taken — the live
    // server's own persist path, used directly since the port itself is
    // silent in this scenario and cannot carry a real round-trip.
    rotate_alice_on_disk(&path);

    let name = finish_login(
        &path,
        WriteRoute::Live,
        &client_config,
        "carol@example.com",
        &tokens("at-carol", "rt-carol", 1_893_456_000_000),
        profile_named("carol@example.com"),
    )
    .await
    .expect("a timed-out live add must still fall back to a file write");
    assert_eq!(name, "carol@example.com");
    assert_the_rotation_survived_and_one_was_added(
        &path,
        "the timed-out live add",
        "alice's rotation",
        "carol",
    );

    std::fs::remove_file(&path).ok();
}

/// THE σ5 EXPERIMENT for the `NoServer` arm specifically — same shape as
/// `finish_login_live_path_does_not_clobber_a_concurrent_rotation` and
/// `finish_login_live_timeout_does_not_clobber_other_accounts`, but for
/// the arm those two do not reach: the proxy the capability probe found
/// alive has exited entirely by the time `post_add_account` connects, so
/// the connection is refused outright (`is_connect()`, not `is_timeout()`
/// or a 404/405). Before this fix, this arm alone still called
/// `persist_via_file` directly regardless of what any other test in this
/// file exercises — a stale client-side snapshot whole-file-written over
/// a rotation that landed on disk in the profile-fetch / stdin-prompt /
/// round-trip window. Watched failing directly: with the `NoServer` arm
/// reverted to `persist_via_file`, this test fails with `left:
/// "rt-alice" right: "rt-alice-ROTATED"`.
#[tokio::test]
async fn finish_login_no_server_does_not_clobber_a_concurrent_rotation() {
    // Bind to grab a free port, then drop the listener immediately: it
    // was live a moment ago (mirroring the capability probe having
    // confirmed it), but by the time `finish_login` runs, nothing is
    // listening and a connect is refused outright.
    let (path, client_config) = alice_on_a_proxy_port("live-noserver-clobber", DEAD_PORT);

    // A rotation lands on disk AFTER that snapshot was taken — the
    // (now-gone) server's own persist path, used directly since the port
    // is silent for the whole test and cannot carry a real round-trip.
    rotate_alice_on_disk(&path);

    let name = finish_login(
        &path,
        WriteRoute::Live,
        &client_config,
        "carol@example.com",
        &tokens("at-carol", "rt-carol", 1_893_456_000_000),
        profile_named("carol@example.com"),
    )
    .await
    .expect("a NoServer live add must still fall back to a surgical write");
    assert_eq!(name, "carol@example.com");
    assert_the_rotation_survived_and_one_was_added(
        &path,
        "the NoServer live add",
        "alice's rotation",
        "carol",
    );

    std::fs::remove_file(&path).ok();
}

/// THE σ5 EXPERIMENT for `WriteRoute::File` itself — the arm
/// `persist_via_file` serves directly, not one of its live-path
/// fallbacks. Reproduces the 2026-08-14 incident: a `tcr login` lands on
/// `File` (no live proxy answers on the guarded port — the exact
/// condition every restart window produces), and while that login's own
/// async window (profile fetch / possible stdin prompt) is open, TWO
/// things change on disk out from under the config snapshot this call
/// was handed: an unrelated top-level key gets added (standing in for
/// `sessionAffinity`, which is exactly this shape — a key with no
/// modelled field in [`Config`] at all, so nothing short of a
/// document-level fresh read can preserve it) and a DIFFERENT account's
/// tokens get rotated by some other writer. Before this fix,
/// `persist_via_file` whole-file-saved the stale snapshot taken before
/// this call, silently reverting both — watched failing directly: with
/// `persist_via_file` reverted to `upsert_account` + whole-file
/// [`config::save`], this test fails on the `sessionAffinity` assertion
/// first (the key is entirely unmodelled, so it cannot survive a
/// snapshot round-trip at all) and, independently, on the rotated
/// refresh token.
#[tokio::test]
async fn finish_login_file_route_does_not_clobber_out_of_band_changes() {
    let path = std::env::temp_dir().join(format!(
        "tcr-oauth-file-route-clobber-{}.json",
        std::process::id()
    ));
    let seed = r#"{ "accounts": [
            { "name": "alice@example.com", "type": "oauth", "accessToken": "at-alice",
              "refreshToken": "rt-alice", "expiresAt": 1893456000000, "priority": 0 }
        ] }"#;
    std::fs::write(&path, seed).unwrap();

    // The stale snapshot: what `login()` loads before its own async
    // window (profile fetch / possible stdin prompt) opens. No live
    // proxy is involved at all — this is the historical `File` route.
    let client_config = load_or_default(&path).unwrap();

    // Out-of-band changes land on disk AFTER that snapshot was taken:
    // an unmodelled top-level key (standing in for `sessionAffinity`)
    // and a rotation of alice's tokens by some other writer.
    let mut doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    doc["sessionAffinity"] = serde_json::json!(true);
    doc["accounts"][0]["accessToken"] = serde_json::json!("at-alice-ROTATED");
    doc["accounts"][0]["refreshToken"] = serde_json::json!("rt-alice-ROTATED");
    std::fs::write(&path, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
    let after_out_of_band = std::fs::read_to_string(&path).unwrap();
    assert!(
        after_out_of_band.contains("rt-alice-ROTATED")
            && after_out_of_band.contains("sessionAffinity"),
        "setup: both out-of-band changes must be on disk first: {after_out_of_band}"
    );

    let name = finish_login(
        &path,
        WriteRoute::File,
        &client_config,
        "carol@example.com",
        &tokens("at-carol", "rt-carol", 1_893_456_000_000),
        profile_named("carol@example.com"),
    )
    .await
    .expect("the File route must still succeed");
    assert_eq!(name, "carol@example.com");

    let final_doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        final_doc["sessionAffinity"],
        serde_json::json!(true),
        "the File route must NOT whole-file-write a stale snapshot back over an \
         out-of-band top-level key: {final_doc}"
    );
    assert_the_rotation_survived_and_one_was_added(
        &path,
        "the File route",
        "an out-of-band rotation",
        "carol",
    );

    std::fs::remove_file(&path).ok();
}

/// A direct exercise of `finish_login`'s `NoRoute` arm. Before this test
/// it was covered only by inference — every existing clobber test in
/// this file drives either `Live`'s success path, `NoServer`, or the
/// timeout/`Unusable` catch-all (`Err(other)`), never the specific
/// unstamped-404 shape `NoRoute` is. Same "older tcr" server as
/// `probe_add_capability_unstamped_404_is_unusable_not_absent`: an empty
/// axum router answers every path, so `post_add_account` sees a 404 with
/// no `ENDPOINT_HEADER` and classifies it structurally as `NoRoute`
/// rather than `Unusable`.
#[tokio::test]
async fn finish_login_no_route_does_not_clobber_other_accounts() {
    let port = empty_router_port();
    let (path, client_config) = alice_on_a_proxy_port("live-noroute", port);

    // A rotation lands on disk AFTER that snapshot was taken — this
    // "older tcr" has no add-account route to have produced it, standing
    // in for any other writer that touched the file in the window, same
    // as every sibling clobber test in this file.
    rotate_alice_on_disk(&path);

    let name = finish_login(
        &path,
        WriteRoute::Live,
        &client_config,
        "dana@example.com",
        &tokens("at-dana", "rt-dana", 1_893_456_000_000),
        profile_named("dana@example.com"),
    )
    .await
    .expect("a NoRoute live add must still fall back to a surgical write");
    assert_eq!(name, "dana@example.com");
    assert_the_rotation_survived_and_one_was_added(
        &path,
        "the NoRoute live add",
        "alice's rotation",
        "dana",
    );

    std::fs::remove_file(&path).ok();
}

/// `persist_via_account_or_file`'s `NotFound` guard: `config::save_account`'s
/// `read_document` opens the file with `fs::read_to_string`, which cannot
/// create one that is not there — so a `NoServer` result reached with NO
/// config file on disk at all must still fall back to `persist_via_file`
/// (the same whole-file write `WriteRoute::File` always used) instead of
/// bailing on the read error. A first-ever login must be able to create
/// the file exactly as it always has.
#[tokio::test]
async fn finish_login_no_server_creates_the_file_when_none_exists() {
    let dead_port = DEAD_PORT;
    let path = std::env::temp_dir().join(format!(
        "tcr-oauth-live-noserver-nofile-{}-{dead_port}.json",
        std::process::id()
    ));
    std::fs::remove_file(&path).ok();
    assert!(!path.exists(), "setup: no config file must exist yet");

    // In-memory only — no file backs this, mirroring what `login()` would
    // hold when there is nothing on disk yet. Must carry `dead_port`
    // explicitly: leaving `proxy.port` at its serde default would send
    // this test's request to whatever real port 3456 happens to hold.
    let client_config: Config =
        serde_json::from_str(&format!(r#"{{ "proxy": {{ "port": {dead_port} }} }}"#)).unwrap();

    let name = finish_login(
        &path,
        WriteRoute::Live,
        &client_config,
        "frank@example.com",
        &tokens("at-frank", "rt-frank", 1_893_456_000_000),
        profile_named("frank@example.com"),
    )
    .await
    .expect("a NoServer live add with no file yet must still create one");
    assert_eq!(name, "frank@example.com");

    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        doc["accounts"][0]["name"],
        serde_json::json!("frank@example.com")
    );

    std::fs::remove_file(&path).ok();
}
