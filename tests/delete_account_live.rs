//! Deleting an account, end to end, the way TcrBar does it: `tcr remove <name>`
//! against a RUNNING proxy, then `tcr status --json`, which is what the panel
//! draws its rows from.
//!
//! The outcome under test (Gil, 2026-10-14: "remove delete account doesnt work
//! sometimes"): after a delete exits 0, the next status read no longer lists the
//! account, with no restart. Before this, the proxy parked the account and kept
//! listing it until it restarted, so a delete of an account that was ALREADY
//! parked changed nothing anyone could see, and a config holding one row the
//! loader quarantines refused every delete after it had already parked the
//! account in the running proxy.
//!
//! # Isolation
//!
//! The proxy runs in THIS process ([`Manager::with_live_refresher`] behind
//! [`tcr::mitm::serve`]) on a kernel-assigned port, so it starts no
//! probe, warm or refresh loop and sends nothing upstream. The `tcr` binary gets
//! a scratch `HOME`, no `XDG_CACHE_HOME`, and `TCR_CLAUDE_CODE_CREDENTIALS`
//! pointing at a file that does not exist, so it can read neither the real
//! config nor the Keychain. Every account is invented. Ordinary fixture rows omit refresh
//! tokens so offline status cannot probe Claude's real usage endpoint. A test-only libc
//! guard rejects the live proxy and non-loopback connections before any data is sent.
//!
//! Every status read asserts `source == "live"`. An offline read builds its rows
//! from the config file, which the delete has already edited, so it would pass
//! these tests whether or not the running proxy dropped anything.

use crate::connect_guard;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use std::sync::Arc;

use serde_json::Value;
use tcr::manager::Manager;
use time::OffsetDateTime;

/// A scratch HOME, its config, and the proxy serving it.
struct Fleet {
    home: tempfile::TempDir,
    config: PathBuf,
    manager: Arc<Manager>,
}

/// Boot a proxy over `accounts` (a JSON array body) plus `extra` top-level
/// keys. `follows_file` false boots it with no config path, which is how a
/// proxy that cannot see the file's edits behaves: the shape of a server built
/// before live removal existed.
async fn boot(accounts: &str, extra: &str, follows_file: bool) -> Fleet {
    boot_after(accounts, extra, follows_file, |_| {}).await
}

/// [`boot`], running `before_load` on the written config's path before the
/// proxy loads it.
async fn boot_after(
    accounts: &str,
    extra: &str,
    follows_file: bool,
    before_load: impl FnOnce(&Path),
) -> Fleet {
    let home = tempfile::tempdir().expect("scratch home");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a scratch port");
    let port = listener.local_addr().expect("local addr").port();
    assert_ne!(port, 3456, "never the live proxy's port");
    let dir = home.path().join(".config");
    std::fs::create_dir_all(&dir).expect("config dir");
    let config = dir.join("tcr.json");
    std::fs::write(
        &config,
        format!(r#"{{ "proxy": {{ "port": {port} }}, "upstream": "http://127.0.0.1:0", "codexUpstream": "http://127.0.0.1:0", {extra} "accounts": [{accounts}] }}"#),
    )
    .expect("write the scratch config");
    before_load(&config);
    let loaded = tcr::config::load(&config).expect("load the scratch config");
    assert_eq!(
        loaded.upstream, "http://127.0.0.1:0",
        "Claude inference stays on the fixture's upstream"
    );
    assert_eq!(
        loaded.codex_upstream, "http://127.0.0.1:0",
        "offline Codex probes must fail closed"
    );
    let manager = Manager::with_live_refresher(loaded, follows_file.then(|| config.clone()));
    let served = Arc::clone(&manager);
    tokio::spawn(async move { tcr::mitm::serve(listener, served, None).await });
    Fleet {
        home,
        config,
        manager,
    }
}

/// Where this fleet's machine keeps its Claude Code login. Absent unless a test
/// writes one, so `tcr` finds no login and never reads the Keychain.
fn claude_code_login(home: &Path) -> PathBuf {
    home.join("claude-code-credentials.json")
}

/// What one `tcr` run said.
struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Run {
    fn says(&self) -> String {
        format!(
            "exit {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.code, self.stdout, self.stderr
        )
    }
}

/// Run the built `tcr` with `args` and `--config`, isolated from the real HOME
/// and the Keychain. Off the async runtime, because the proxy it talks to is
/// served by this same runtime.
async fn tcr(fleet: &Fleet, args: &[&str]) -> Run {
    let home = fleet.home.path().to_path_buf();
    let config = fleet.config.clone();
    let args: Vec<String> = args.iter().map(ToString::to_string).collect();
    tokio::task::spawn_blocking(move || {
        let mut command = crate::tcr_env::tcr(&home);
        command
            .args(&args)
            .arg("--config")
            .arg(&config)
            .env("CODEX_HOME", home.join(".codex"))
            .env("TCR_CODEX_AUTH", home.join("no-such-codex-auth.json"))
            .env("TCR_CLAUDE_CODE_CREDENTIALS", claude_code_login(&home))
            .env("NO_COLOR", "1")
            .current_dir(&home);
        let guard = connect_guard::ConnectGuard::attach(&mut command, &home);
        let out = command.output().expect("run tcr");
        guard.assert_no_connections();
        Run {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    })
    .await
    .expect("tcr task")
}

/// The panel's view: `tcr status --json`, one poll. Asserts the answer came
/// from the running proxy (see the module doc).
async fn poll(fleet: &Fleet) -> Vec<Value> {
    let run = tcr(fleet, &["status", "--json"]).await;
    assert_eq!(run.code, Some(0), "status must answer: {}", run.says());
    let rows: Vec<Value> = serde_json::from_str(&run.stdout)
        .unwrap_or_else(|e| panic!("status --json is a row array ({e}): {}", run.says()));
    for row in &rows {
        assert_eq!(
            row["source"], "live",
            "this read must come from the running proxy, not the file: {row}"
        );
    }
    rows
}

fn names(rows: &[Value]) -> Vec<String> {
    rows.iter()
        .map(|r| r["name"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// The config file's account names, read raw so a quarantined entry counts.
fn file_names(path: &Path) -> Vec<String> {
    let doc: Value =
        serde_json::from_str(&std::fs::read_to_string(path).expect("read config")).expect("json");
    doc["accounts"]
        .as_array()
        .expect("accounts array")
        .iter()
        .map(|a| a["name"].as_str().unwrap_or_default().to_string())
        .collect()
}

fn oauth(name: &str, extra: &str) -> String {
    format!(
        r#"{{ "name": "{name}", "type": "oauth", "accessToken": "at-{name}",
             "expiresAt": 4102444800000 {extra} }}"#
    )
}

/// Delete `name` through the CLI and require the next poll to show exactly
/// `left`, from the running proxy, with the file agreeing.
async fn delete_and_expect(fleet: &Fleet, name: &str, left: &[&str]) -> Run {
    let before = names(&poll(fleet).await);
    assert!(
        before.iter().any(|n| n == name),
        "the fixture must list '{name}' before the delete: {before:?}"
    );
    let run = tcr(fleet, &["remove", name]).await;
    assert_eq!(run.code, Some(0), "the delete must succeed: {}", run.says());
    let after = names(&poll(fleet).await);
    assert_eq!(
        after,
        left,
        "one poll after the delete, the running proxy lists exactly the survivors \
         ({})",
        run.says()
    );
    let on_disk: HashSet<String> = file_names(&fleet.config).into_iter().collect();
    assert!(
        !on_disk.contains(name),
        "the file no longer holds '{name}': {on_disk:?}"
    );
    run
}

// --- account states -----------------------------------------------------------

#[tokio::test]
async fn a_live_account_leaves_the_running_proxy_within_one_poll() {
    let fleet = boot(
        &[oauth("alice@example.com", ""), oauth("bob@example.com", "")].join(","),
        "",
        true,
    )
    .await;
    delete_and_expect(&fleet, "alice@example.com", &["bob@example.com"]).await;
}

/// The case Gil suspected. A parked account already reads as parked, so a
/// delete that only parks it in the proxy changes nothing on screen.
#[tokio::test]
async fn a_parked_account_leaves_the_running_proxy_within_one_poll() {
    let fleet = boot(
        &[
            oauth("alice@example.com", r#", "disabled": true"#),
            oauth("bob@example.com", ""),
        ]
        .join(","),
        "",
        true,
    )
    .await;
    delete_and_expect(&fleet, "alice@example.com", &["bob@example.com"]).await;
}

/// Near its limit, out of tokens, and never measured: quota state must not
/// change what a delete does.
#[tokio::test]
async fn quota_state_does_not_change_a_delete() {
    let fleet = boot(
        &[
            oauth("near@example.com", ""),
            oauth("spent@example.com", ""),
            oauth("never@example.com", ""),
            oauth("bob@example.com", ""),
        ]
        .join(","),
        "",
        true,
    )
    .await;
    let reset = tcr::now_ms() + 3_600_000;
    for (idx, used) in [(0, 0.9), (1, 1.0)] {
        fleet.manager.apply_usage(
            idx,
            &tcr::probe::Usage {
                five_hour: Some(tcr::probe::UsageBucket {
                    utilization: Some(used),
                    reset_at_ms: Some(reset),
                }),
                seven_day: None,
                seven_day_oi: None,
                extra_usage_usd: None,
                codex: None,
            },
        );
    }
    delete_and_expect(
        &fleet,
        "near@example.com",
        &["spent@example.com", "never@example.com", "bob@example.com"],
    )
    .await;
    delete_and_expect(
        &fleet,
        "spent@example.com",
        &["never@example.com", "bob@example.com"],
    )
    .await;
    delete_and_expect(&fleet, "never@example.com", &["bob@example.com"]).await;
}

/// The status endpoint zips each row with its threshold by position, so the
/// threshold list must drop the deleted row exactly where the row list does,
/// or every account after it reads its neighbour's threshold.
#[tokio::test]
async fn a_deleted_row_in_the_middle_shifts_no_other_rows_threshold() {
    let fleet = boot(
        &[
            oauth("alice@example.com", r#", "switchThreshold": 0.5"#),
            oauth("bob@example.com", r#", "switchThreshold": 0.7"#),
            oauth("carol@example.com", r#", "switchThreshold": 0.9"#),
        ]
        .join(","),
        "",
        true,
    )
    .await;
    // carol served last: `current` names her, by position in the list.
    fleet.manager.record_served(
        2,
        OffsetDateTime::now_utc(),
        None,
        tcr::stats::SessionKind::Fallback,
    );
    delete_and_expect(
        &fleet,
        "bob@example.com",
        &["alice@example.com", "carol@example.com"],
    )
    .await;
    let snapshot = fleet.manager.snapshot(OffsetDateTime::now_utc());
    assert_eq!(
        snapshot.current,
        Some(1),
        "carol is now the second row listed"
    );
    let listed = snapshot.accounts;
    let thresholds = fleet.manager.thresholds();
    let pairs: Vec<(&str, f64)> = listed
        .iter()
        .map(|a| a.name.as_str())
        .zip(thresholds.iter().copied())
        .collect();
    assert_eq!(
        pairs,
        vec![("alice@example.com", 0.5), ("carol@example.com", 0.9)],
        "and no more thresholds than rows: {thresholds:?}"
    );
    assert_eq!(thresholds.len(), listed.len());
}

/// A setup-token row: an access token and no refresh token.
#[tokio::test]
async fn a_setup_token_account_leaves_the_running_proxy() {
    let fleet = boot(
        &[
            r#"{ "name": "seat@example.com", "type": "oauth", "accessToken": "at-seat" }"#
                .to_string(),
            oauth("bob@example.com", ""),
        ]
        .join(","),
        "",
        true,
    )
    .await;
    delete_and_expect(&fleet, "seat@example.com", &["bob@example.com"]).await;
}

/// One row the loader quarantines anywhere in the file: an API-key row, a row
/// whose `type` is not a string, a row with a foreign `upstream`. The delete of
/// a DIFFERENT, healthy account must still work, must not have parked anything
/// on its way to failing, and must leave the quarantined rows byte for byte.
#[tokio::test]
async fn a_quarantined_row_elsewhere_in_the_file_does_not_block_a_delete() {
    let quarantined = [
        r#"{ "name": "key@example.com", "type": "apikey", "accessToken": "k-1" }"#,
        r#"{ "name": "typo@example.com", "type": 7, "accessToken": "at-typo" }"#,
        r#"{ "name": "far@example.com", "type": "oauth", "accessToken": "at-far",
             "upstream": "https://llm.example.net" }"#,
    ];
    for row in quarantined {
        let fleet = boot(
            &[
                oauth("alice@example.com", ""),
                row.to_string(),
                oauth("bob@example.com", ""),
            ]
            .join(","),
            "",
            true,
        )
        .await;
        let kept_before: Value = serde_json::from_str(
            &serde_json::from_str::<Value>(&std::fs::read_to_string(&fleet.config).unwrap())
                .unwrap()["accounts"][1]
                .to_string(),
        )
        .unwrap();
        delete_and_expect(&fleet, "alice@example.com", &["bob@example.com"]).await;
        let doc: Value =
            serde_json::from_str(&std::fs::read_to_string(&fleet.config).unwrap()).unwrap();
        assert_eq!(
            doc["accounts"][0], kept_before,
            "the quarantined row survives the delete untouched ({row})"
        );
        assert!(
            !fleet.manager.snapshot(OffsetDateTime::now_utc()).accounts[0].disabled,
            "bob was never parked along the way"
        );
    }
}

/// A failed delete has no side effect: the file is byte-identical and the
/// running proxy did not park the account.
#[tokio::test]
async fn a_delete_that_fails_parks_nothing() {
    let fleet = boot(
        &[oauth("alice@example.com", ""), oauth("bob@example.com", "")].join(","),
        "",
        true,
    )
    .await;
    let before = std::fs::read_to_string(&fleet.config).unwrap();
    let run = tcr(&fleet, &["remove", "nobody@example.com"]).await;
    assert_ne!(run.code, Some(0), "{}", run.says());
    assert!(
        run.stderr.contains("nobody@example.com"),
        "the refusal names the query: {}",
        run.says()
    );
    assert_eq!(before, std::fs::read_to_string(&fleet.config).unwrap());
    let rows = poll(&fleet).await;
    assert!(rows.iter().all(|r| r["disabled"] != true), "{rows:?}");
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
}

/// Delete `bob` with the config's directory read-only, and return what the
/// delete said and what the next poll listed for bob.
#[cfg(unix)]
async fn delete_bob_read_only(fleet: &Fleet) -> (Run, Value) {
    let dir = fleet.config.parent().expect("config dir").to_path_buf();
    set_mode(&dir, 0o555);
    let before = std::fs::read_to_string(&fleet.config).unwrap();
    let run = tcr(fleet, &["remove", "bob@example.com"]).await;
    let rows = poll(fleet).await;
    set_mode(&dir, 0o755);
    assert_ne!(
        run.code,
        Some(0),
        "the file cannot take this edit: {}",
        run.says()
    );
    assert_eq!(before, std::fs::read_to_string(&fleet.config).unwrap());
    let bob = rows
        .iter()
        .find(|r| r["name"] == "bob@example.com")
        .cloned()
        .expect("bob is still listed");
    (run, bob)
}

/// A delete the FILE refuses before any write must not have parked the account
/// first. Two rows share a name and the directory cannot be written, so the
/// loader's rename cannot land and the delete refuses the file. It used to
/// park the account in the running proxy, then find that out.
#[cfg(unix)]
#[tokio::test]
async fn a_delete_the_file_refuses_parks_nothing() {
    let fleet = boot_after(
        &[
            oauth(
                "carol@example.com",
                r#", "accountUuid": "33333333-3333-3333-3333-333333333333""#,
            ),
            oauth(
                "carol@example.com",
                r#", "accountUuid": "44444444-4444-4444-4444-444444444444""#,
            ),
            oauth("bob@example.com", ""),
        ]
        .join(","),
        "",
        true,
        |config| set_mode(config.parent().expect("config dir"), 0o555),
    )
    .await;
    let (run, bob) = delete_bob_read_only(&fleet).await;
    assert!(run.stderr.contains("sharing a name"), "{}", run.says());
    assert_eq!(
        bob["disabled"],
        false,
        "a refused delete parked nothing: {}",
        run.says()
    );
}

/// The write itself fails, after the running proxy already parked the account.
/// The park is undone, so a failed delete leaves the account as it found it.
#[cfg(unix)]
#[tokio::test]
async fn a_delete_whose_write_fails_unparks_the_account() {
    let fleet = boot(
        &[oauth("alice@example.com", ""), oauth("bob@example.com", "")].join(","),
        "",
        true,
    )
    .await;
    let (run, bob) = delete_bob_read_only(&fleet).await;
    assert_eq!(
        bob["disabled"],
        false,
        "the park was undone: {}",
        run.says()
    );
}

// --- placement ----------------------------------------------------------------

#[tokio::test]
async fn an_account_in_two_groups_leaves_both() {
    let fleet = boot(
        &[
            oauth("alice@example.com", r#", "groups": ["red", "blue"]"#),
            oauth("bob@example.com", r#", "groups": ["red"]"#),
        ]
        .join(","),
        "",
        true,
    )
    .await;
    delete_and_expect(&fleet, "alice@example.com", &["bob@example.com"]).await;
    let rows = poll(&fleet).await;
    assert_eq!(rows[0]["groups"], serde_json::json!(["red"]));
}

/// The control account. Its routing bypasses `disabled` on purpose, so parking
/// it never stopped control-plane traffic; deleting it must.
#[tokio::test]
async fn the_control_account_leaves_and_stops_taking_control_traffic() {
    let fleet = boot(
        &[oauth("alice@example.com", ""), oauth("bob@example.com", "")].join(","),
        r#""controlAccount": "alice@example.com","#,
        true,
    )
    .await;
    assert_eq!(
        fleet.manager.control_name().as_deref(),
        Some("alice@example.com")
    );
    delete_and_expect(&fleet, "alice@example.com", &["bob@example.com"]).await;
    assert_eq!(
        fleet.manager.control_name(),
        None,
        "a deleted control account is not the control account"
    );
    let picked = fleet.manager.select(
        &HashSet::new(),
        OffsetDateTime::now_utc(),
        None,
        None,
        "/api/oauth/profile",
        None,
    );
    assert_eq!(
        picked,
        Some(1),
        "control-plane traffic goes to bob, never the deleted row"
    );
}

/// `lockAccount` serves one account whatever its state. Deleted, it is no lock:
/// rotation carries on over the accounts that are left.
#[tokio::test]
async fn a_deleted_locked_account_no_longer_takes_traffic() {
    let fleet = boot(
        &[oauth("alice@example.com", ""), oauth("bob@example.com", "")].join(","),
        r#""lockAccount": "alice@example.com","#,
        true,
    )
    .await;
    assert_eq!(
        fleet.manager.locked_account_name().as_deref(),
        Some("alice@example.com")
    );
    delete_and_expect(&fleet, "alice@example.com", &["bob@example.com"]).await;
    assert_eq!(fleet.manager.locked_account_name(), None);
    let picked = fleet.manager.select(
        &HashSet::new(),
        OffsetDateTime::now_utc(),
        None,
        None,
        "/v1/messages",
        None,
    );
    assert_eq!(picked, Some(1), "bob serves; the deleted lock does not");
}

// --- coming back ----------------------------------------------------------------

/// A delete by hand edit is a delete too, and undoing the edit brings the row
/// back as the file has it.
#[tokio::test]
async fn a_row_deleted_by_hand_leaves_and_comes_back_when_restored() {
    let fleet = boot(
        &[
            oauth("alice@example.com", r#", "disabled": true"#),
            oauth("bob@example.com", ""),
        ]
        .join(","),
        "",
        true,
    )
    .await;
    let original = std::fs::read_to_string(&fleet.config).unwrap();
    let mut doc: Value = serde_json::from_str(&original).unwrap();
    let alice = doc["accounts"].as_array_mut().unwrap().remove(0);
    std::fs::write(&fleet.config, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
    assert_eq!(names(&poll(&fleet).await), vec!["bob@example.com"]);

    doc["accounts"].as_array_mut().unwrap().insert(0, alice);
    std::fs::write(&fleet.config, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
    let rows = poll(&fleet).await;
    assert_eq!(names(&rows), vec!["alice@example.com", "bob@example.com"]);
    assert_eq!(rows[0]["disabled"], true, "back as the file has it: parked");
}

/// A login of the deleted account, through the live add route, brings it back
/// serving and writes it back to the file.
#[tokio::test]
async fn a_login_after_a_delete_brings_the_account_back_serving() {
    let fleet = boot(
        &[
            oauth("alice@example.com", r#", "disabled": true"#),
            oauth("bob@example.com", ""),
        ]
        .join(","),
        "",
        true,
    )
    .await;
    delete_and_expect(&fleet, "alice@example.com", &["bob@example.com"]).await;
    let login: tcr::config::Account =
        serde_json::from_str(&oauth("alice@example.com", "")).unwrap();
    fleet.manager.add_or_update_account(login);
    let rows = poll(&fleet).await;
    assert_eq!(names(&rows), vec!["alice@example.com", "bob@example.com"]);
    assert_eq!(rows[0]["disabled"], false, "a fresh login serves: {rows:?}");
    assert!(file_names(&fleet.config).contains(&"alice@example.com".to_string()));
}

#[tokio::test]
async fn deletion_rows_cannot_probe_a_live_claude_usage_endpoint() {
    let fleet = boot(&oauth("alice@example.com", ""), "", true).await;
    assert!(
        fleet.manager.probeable_indices().is_empty(),
        "an offline deletion fixture must not probe Claude's fixed real usage endpoint"
    );
}

// --- fleet ----------------------------------------------------------------------

#[tokio::test]
async fn the_last_remaining_account_leaves() {
    let fleet = boot(&oauth("alice@example.com", ""), "", true).await;
    delete_and_expect(&fleet, "alice@example.com", &[]).await;
}

/// The last account, on a machine that also holds a Claude Code login. The
/// panel's next poll runs `tcr status`, which imports that login into an EMPTY
/// fleet; a fleet emptied by a delete must stay empty, or the delete undoes
/// itself one poll later.
#[tokio::test]
async fn the_last_account_stays_gone_when_the_machine_has_a_claude_code_login() {
    let fleet = boot(&oauth("alice@example.com", ""), "", true).await;
    std::fs::write(
        claude_code_login(fleet.home.path()),
        r#"{ "claudeAiOauth": { "accessToken": "at-fake-claude-code",
             "refreshToken": "rt-fake-claude-code", "expiresAt": 4102444800000,
             "scopes": ["user:inference", "user:profile"], "subscriptionType": "max" } }"#,
    )
    .expect("write the fake Claude Code login");
    delete_and_expect(&fleet, "alice@example.com", &[]).await;
    let again = names(&poll(&fleet).await);
    assert!(
        again.is_empty(),
        "a second poll imports nothing either: {again:?}"
    );
    assert!(file_names(&fleet.config).is_empty());
}

/// A session pinned to the deleted account moves on its next request.
#[tokio::test]
async fn an_account_holding_session_pins_leaves_and_its_sessions_move() {
    let fleet = boot(
        &[
            oauth("alice@example.com", r#", "priority": -1"#),
            oauth("bob@example.com", ""),
        ]
        .join(","),
        "",
        true,
    )
    .await;
    let session = Some(0xA11CE_u64);
    let now = OffsetDateTime::now_utc();
    let first = fleet
        .manager
        .select(&HashSet::new(), now, None, session, "/v1/messages", None);
    assert_eq!(first, Some(0), "the fixture pins the session on alice");
    delete_and_expect(&fleet, "alice@example.com", &["bob@example.com"]).await;
    let next = fleet.manager.select(
        &HashSet::new(),
        OffsetDateTime::now_utc(),
        None,
        session,
        "/v1/messages",
        None,
    );
    assert_eq!(next, Some(1), "the pinned session moves to bob");
}

/// One email in two orgs: two rows, two names. Deleting one leaves the other.
#[tokio::test]
async fn one_email_in_two_orgs_deletes_only_the_named_row() {
    let fleet = boot(
        &[
            oauth(
                "alice@example.com",
                r#", "orgUuid": "11111111-1111-1111-1111-111111111111", "orgName": "Org A""#,
            ),
            oauth(
                "alice@example.com/org-b",
                r#", "orgUuid": "22222222-2222-2222-2222-222222222222", "orgName": "Org B""#,
            ),
        ]
        .join(","),
        "",
        true,
    )
    .await;
    delete_and_expect(&fleet, "alice@example.com/org-b", &["alice@example.com"]).await;
    delete_and_expect(&fleet, "alice@example.com", &[]).await;
}

/// A label that is not an email, beside one that is.
#[tokio::test]
async fn a_plain_label_and_an_email_label_both_delete() {
    let fleet = boot(
        &[
            oauth("team-seat", ""),
            oauth("alice@example.com", ""),
            oauth("bob@example.com", ""),
        ]
        .join(","),
        "",
        true,
    )
    .await;
    delete_and_expect(
        &fleet,
        "team-seat",
        &["alice@example.com", "bob@example.com"],
    )
    .await;
    delete_and_expect(&fleet, "alice@example.com", &["bob@example.com"]).await;
}

// --- server ---------------------------------------------------------------------

/// Nothing listening: the file is the whole fleet, and the delete edits it.
#[tokio::test]
async fn with_no_server_running_the_file_edit_is_the_delete() {
    let home = tempfile::tempdir().expect("scratch home");
    let port = tcr::test_support::loopback::unused_port();
    let dir = home.path().join(".config");
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("tcr.json");
    std::fs::write(
        &config,
        format!(
            r#"{{ "proxy": {{ "port": {port} }}, "accounts": [{}] }}"#,
            [oauth("alice@example.com", ""), oauth("bob@example.com", "")].join(",")
        ),
    )
    .unwrap();
    let out = crate::tcr_env::tcr(home.path())
        .args(["remove", "alice@example.com", "--config"])
        .arg(&config)
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(file_names(&config), vec!["bob@example.com"]);
}

/// A proxy that cannot follow the file (built before live removal): the delete
/// still lands in the file and stops the account serving, and says, on the
/// stderr TcrBar draws, that the proxy keeps LISTING it until it restarts.
#[tokio::test]
async fn a_server_that_cannot_drop_the_row_is_named_in_the_output() {
    let fleet = boot(
        &[oauth("alice@example.com", ""), oauth("bob@example.com", "")].join(","),
        "",
        false,
    )
    .await;
    let run = tcr(&fleet, &["remove", "alice@example.com"]).await;
    assert_eq!(run.code, Some(0), "{}", run.says());
    assert_eq!(file_names(&fleet.config), vec!["bob@example.com"]);
    let rows = poll(&fleet).await;
    assert_eq!(names(&rows), vec!["alice@example.com", "bob@example.com"]);
    assert_eq!(rows[0]["disabled"], true, "it stopped serving: {rows:?}");
    assert!(
        run.stderr.contains("until it restarts"),
        "the delete must say the row stays listed: {}",
        run.says()
    );
}

// --- remove from group ----------------------------------------------------------

/// `tcr group rm` shows on the next poll, quarantined row or not.
#[tokio::test]
async fn remove_from_group_shows_on_the_next_poll() {
    let fleet = boot(
        &[
            oauth("alice@example.com", r#", "groups": ["red", "blue"]"#),
            r#"{ "name": "key@example.com", "type": "apikey", "accessToken": "k-1" }"#.to_string(),
        ]
        .join(","),
        "",
        true,
    )
    .await;
    let run = tcr(&fleet, &["group", "rm", "red", "alice@example.com"]).await;
    assert_eq!(run.code, Some(0), "{}", run.says());
    let rows = poll(&fleet).await;
    assert_eq!(rows[0]["groups"], serde_json::json!(["blue"]), "{rows:?}");
    let run = tcr(&fleet, &["group", "rm", "blue", "alice@example.com"]).await;
    assert_eq!(run.code, Some(0), "{}", run.says());
    let rows = poll(&fleet).await;
    assert!(
        rows[0]["groups"].as_array().is_none_or(Vec::is_empty),
        "{rows:?}"
    );
}

// --- Codex rows: a ChatGPT user in a workspace -------------------------------------

/// A Codex row in the shape an import writes: `accountUuid` the ChatGPT user, `orgUuid` the
/// workspace, plus any `extra` keys.
fn codex(name: &str, user: &str, workspace: &str, extra: &str) -> String {
    oauth(
        name,
        &format!(
            r#", "provider": "codex", "credentialVersion": 2, "accountUuid": "{user}",
               "orgUuid": "{workspace}" {extra}"#
        ),
    )
}

/// One delete takes one seat, and only that one: the Claude twin of the same person stays
/// (another provider), the other seat of the same workspace stays (another ChatGPT user), and
/// the same user's other workspace stays (another workspace).
#[tokio::test]
async fn a_codex_delete_takes_only_that_seat() {
    let fleet = boot(
        &[
            oauth(
                "alice@example.com",
                r#", "accountUuid": "11111111-1111-4111-8111-111111111111""#,
            ),
            codex(
                "alice@example.com/codex",
                "55555555-5555-4555-8555-555555555555",
                "66666666-6666-4666-8666-666666666666",
                "",
            ),
            codex(
                "bob@example.com",
                "77777777-7777-4777-8777-777777777777",
                "66666666-6666-4666-8666-666666666666",
                "",
            ),
            codex(
                "erin@example.com",
                "88888888-8888-4888-8888-888888888888",
                "99999999-9999-4999-8999-999999999999",
                "",
            ),
            codex(
                "work",
                "88888888-8888-4888-8888-888888888888",
                "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                "",
            ),
        ]
        .join(","),
        "",
        true,
    )
    .await;
    delete_and_expect(
        &fleet,
        "alice@example.com/codex",
        &[
            "alice@example.com",
            "bob@example.com",
            "erin@example.com",
            "work",
        ],
    )
    .await;
    delete_and_expect(
        &fleet,
        "work",
        &["alice@example.com", "bob@example.com", "erin@example.com"],
    )
    .await;
}

/// A Codex row saved before the ChatGPT user was split from the workspace (the workspace in
/// `accountUuid`, no `orgUuid`) is matched by no login of the new shape, so a login after its
/// delete adds a row of its own, serving, with its folder. Deleted again, the same login
/// revives that row in place, with the folder it brings and no login error.
#[tokio::test]
async fn an_old_shape_codex_row_is_replaced_live_and_comes_back_after_a_delete() {
    let fleet = boot(
        &[
            oauth("dana@example.com", ""),
            oauth(
                "carol@example.com",
                r#", "provider": "codex", "credentialVersion": 2,
                   "accountUuid": "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb""#,
            ),
        ]
        .join(","),
        "",
        true,
    )
    .await;
    delete_and_expect(&fleet, "carol@example.com", &["dana@example.com"]).await;

    let login = |codex_home: &str| -> tcr::config::Account {
        serde_json::from_str(&codex(
            "carol@example.com",
            "cccccccc-cccc-4ccc-8ccc-cccccccccccc",
            "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
            &format!(r#", "codexHome": "{codex_home}""#),
        ))
        .expect("a Codex login")
    };
    assert!(matches!(
        fleet
            .manager
            .add_or_update_account(login("/Users/example/.codex")),
        tcr::manager::AddAccountOutcome::Added { .. }
    ));
    let rows = poll(&fleet).await;
    assert_eq!(names(&rows), vec!["dana@example.com", "carol@example.com"]);
    assert_eq!(rows[1]["disabled"], false, "{rows:?}");
    assert_eq!(rows[1]["codexHome"], "/Users/example/.codex", "{rows:?}");
    let doc: Value =
        serde_json::from_str(&std::fs::read_to_string(&fleet.config).unwrap()).unwrap();
    let carols: Vec<&Value> = doc["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["name"] == "carol@example.com")
        .collect();
    assert_eq!(carols.len(), 1, "{doc}");
    assert_eq!(
        carols[0]["accountUuid"],
        "cccccccc-cccc-4ccc-8ccc-cccccccccccc"
    );
    assert_eq!(carols[0]["orgUuid"], "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb");

    delete_and_expect(&fleet, "carol@example.com", &["dana@example.com"]).await;
    assert!(matches!(
        fleet
            .manager
            .add_or_update_account(login("/Users/example/.codex-b")),
        tcr::manager::AddAccountOutcome::Updated { .. }
    ));
    let rows = poll(&fleet).await;
    assert_eq!(names(&rows), vec!["dana@example.com", "carol@example.com"]);
    assert_eq!(rows[1]["disabled"], false, "{rows:?}");
    assert_eq!(rows[1]["codexHome"], "/Users/example/.codex-b", "{rows:?}");
    assert!(rows[1].get("loginError").is_none(), "{rows:?}");
}

/// A Codex row taken out of the file by hand and put back with another folder comes back with
/// the file's folder: the file's row is the truth again.
#[tokio::test]
async fn a_codex_row_restored_by_hand_takes_the_files_codex_home() {
    let fleet = boot(
        &[
            codex(
                "erin@example.com",
                "88888888-8888-4888-8888-888888888888",
                "99999999-9999-4999-8999-999999999999",
                r#", "codexHome": "/Users/example/.codex""#,
            ),
            oauth("dana@example.com", ""),
        ]
        .join(","),
        "",
        true,
    )
    .await;
    let original = std::fs::read_to_string(&fleet.config).unwrap();
    let mut doc: Value = serde_json::from_str(&original).unwrap();
    let mut erin = doc["accounts"].as_array_mut().unwrap().remove(0);
    std::fs::write(&fleet.config, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
    assert_eq!(names(&poll(&fleet).await), vec!["dana@example.com"]);

    erin["codexHome"] = Value::from("/Users/example/.codex-b");
    doc["accounts"].as_array_mut().unwrap().insert(0, erin);
    std::fs::write(&fleet.config, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
    let rows = poll(&fleet).await;
    assert_eq!(names(&rows), vec!["erin@example.com", "dana@example.com"]);
    assert_eq!(rows[0]["codexHome"], "/Users/example/.codex-b", "{rows:?}");
}
