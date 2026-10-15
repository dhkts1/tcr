//! Proves the point of the feature: a machine that already has a Claude Code
//! login does not need `tcr login` at all. The first verb that finds an empty
//! fleet imports that login and carries on.
//!
//! End-to-end against the BUILT BINARY, because the defect this guards is not
//! in any one function — it is in whether the verbs are WIRED to the import at
//! all. Every unit test in `src/oauth.rs` stays green with the call site
//! deleted from `cli::list_accounts`.
//!
//! # Isolation
//!
//! Three things are pinned on the spawned [`Command`], never on this test
//! process:
//!
//! * `HOME` → a fresh [`tempfile::TempDir`], so the real
//!   `~/.config/teamclaude.json` (working credentials for real accounts) is
//!   never read or written.
//! * `TCR_CLAUDE_CODE_CREDENTIALS` → a fake credentials file inside that
//!   tempdir. A scratch `HOME` does NOT scratch the login Keychain: without
//!   this, the child would read the developer's real Claude Code login and
//!   import it into the config under test.
//! * `proxy.port` in the config the test writes → a port nothing is listening
//!   on, obtained by binding and releasing one. The import probes that port for
//!   a live account-add route before writing (`oauth::login_route`), and this
//!   machine may well have a real proxy on the default 3456 — which this test
//!   must neither talk to nor add an account to.
//!
//! That last pin is why the config exists at all here rather than being created
//! by the run: the bridge's "no config file" case cannot pin a port, so it
//! would probe whatever is on 3456. The branch under test is the same one
//! either way — `load_or_init` returning a config with zero accounts in it.
use std::path::{Path, PathBuf};
use std::process::Command;

/// A fake Claude Code credentials document — obviously-fake values, and the
/// key names measured on a real install.
const FAKE_CREDENTIALS: &str = r#"{
  "mcpOAuth": {},
  "claudeAiOauth": {
    "accessToken": "at-fake-claude-code-access",
    "refreshToken": "rt-fake-claude-code-refresh",
    "expiresAt": 1789000000000,
    "refreshTokenExpiresAt": 1791000000000,
    "scopes": ["user:inference", "user:profile"],
    "subscriptionType": "max"
  }
}"#;

fn config_path_in(home: &Path) -> PathBuf {
    home.join(".config").join("teamclaude.json")
}

/// A port nothing is listening on: bound to learn the number, then released.
fn dead_port() -> u16 {
    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").expect("binding an ephemeral port must work");
    let port = listener
        .local_addr()
        .expect("a bound listener has an address")
        .port();
    drop(listener);
    port
}

/// A scratch `HOME` holding a zero-account config on a dead port and a fake
/// Claude Code credentials file. Returns the tempdir and the credentials path.
fn scratch_home() -> (tempfile::TempDir, PathBuf) {
    let home = tempfile::tempdir().expect("a scratch HOME must be creatable");
    let config = config_path_in(home.path());
    std::fs::create_dir_all(config.parent().expect("the config has a parent"))
        .expect("creating the scratch .config dir");
    std::fs::write(
        &config,
        format!(
            r#"{{ "proxy": {{ "port": {} }}, "accounts": [] }}"#,
            dead_port()
        ),
    )
    .expect("writing the scratch config");

    let credentials = home.path().join(".claude").join(".credentials.json");
    std::fs::create_dir_all(credentials.parent().expect("it has a parent"))
        .expect("creating the scratch .claude dir");
    std::fs::write(&credentials, FAKE_CREDENTIALS).expect("writing the fake credentials");
    (home, credentials)
}

/// Run `tcr <args…>` against a scratch `HOME`, with the credential store
/// pinned to `credentials`. Returns (exit code, stdout, stderr).
fn run_tcr(home: &Path, credentials: &Path, args: &[&str]) -> (Option<i32>, String, String) {
    let bin = env!("CARGO_BIN_EXE_tcr");
    let out = Command::new(bin)
        .args(args)
        .env("HOME", home)
        .env("TCR_CLAUDE_CODE_CREDENTIALS", credentials)
        .env_remove("XDG_CACHE_HOME")
        .output()
        .unwrap_or_else(|err| panic!("spawning the built tcr binary ({bin}) failed: {err}"));
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn accounts_of(config: &Path) -> Vec<serde_json::Value> {
    let written = std::fs::read_to_string(config)
        .unwrap_or_else(|err| panic!("no config at {}: {err}", config.display()));
    let parsed: serde_json::Value =
        serde_json::from_str(&written).unwrap_or_else(|err| panic!("unparseable config: {err}"));
    parsed
        .get("accounts")
        .and_then(|a| a.as_array())
        .cloned()
        .unwrap_or_default()
}

/// Mutation note: delete the `import_claude_code_login_if_empty` call from
/// `cli::list_accounts` and this fails on the account count — the config keeps
/// the empty `accounts` array it started with.
#[test]
fn a_first_run_imports_the_machines_claude_code_login() {
    let (home, credentials) = scratch_home();
    let config = config_path_in(home.path());
    assert!(
        accounts_of(&config).is_empty(),
        "the scratch config must start empty — this test proves the FIRST run"
    );

    let (code, stdout, stderr) = run_tcr(home.path(), &credentials, &["accounts"]);
    assert_eq!(
        code,
        Some(0),
        "`tcr accounts` must still exit 0 while importing. stdout={stdout} stderr={stderr}"
    );

    let accounts = accounts_of(&config);
    assert_eq!(
        accounts.len(),
        1,
        "the import must leave exactly one account behind, got {}",
        accounts.len()
    );
    assert_eq!(
        accounts[0].get("refreshToken"),
        Some(&serde_json::json!("rt-fake-claude-code-refresh")),
        "the imported row must carry the credential's refresh token"
    );
    assert_eq!(
        accounts[0].get("expiresAt"),
        Some(&serde_json::json!(1_789_000_000_000i64)),
        "the imported row's expiry must be the credential's own `expiresAt`, in ms"
    );

    assert!(
        stderr.contains("imported") && stderr.contains("Claude Code login"),
        "the import must say so, on stderr: {stderr}"
    );
    assert!(
        stderr.contains("log in again"),
        "the single-use refresh-token consequence must be stated: {stderr}"
    );
    assert!(
        !stderr.contains("no accounts configured"),
        "a run that imported an account must not also print the empty-fleet hint: {stderr}"
    );
    assert!(
        !stdout.contains("rt-fake-claude-code-refresh")
            && !stderr.contains("rt-fake-claude-code-refresh"),
        "no output may ever carry token material"
    );
}

/// Idempotence, which is what keeps a deliberate `tcr remove` from being undone
/// behind the user's back: the second run has accounts, so it imports nothing.
#[test]
fn a_second_run_imports_nothing() {
    let (home, credentials) = scratch_home();
    let config = config_path_in(home.path());

    let (first, _, _) = run_tcr(home.path(), &credentials, &["accounts"]);
    assert_eq!(first, Some(0), "the first run must exit 0");
    let after_first = std::fs::read(&config).expect("the config is there after the first run");

    let (second, _, stderr) = run_tcr(home.path(), &credentials, &["accounts"]);
    assert_eq!(second, Some(0), "the second run must exit 0");
    assert!(
        !stderr.contains("imported"),
        "a config that already has an account must not be imported into again: {stderr}"
    );
    assert_eq!(
        std::fs::read(&config).expect("the config is still there"),
        after_first,
        "a non-importing run must leave the file byte-for-byte alone"
    );
    assert_eq!(accounts_of(&config).len(), 1, "still exactly one account");
}

/// No login on the machine: the ordinary first-run hint, and nothing written.
#[test]
fn no_claude_code_login_leaves_the_first_run_hint_alone() {
    let (home, _) = scratch_home();
    let config = config_path_in(home.path());
    let absent = home.path().join("no-such-credentials.json");

    let (code, _, stderr) = run_tcr(home.path(), &absent, &["accounts"]);
    assert_eq!(code, Some(0), "`tcr accounts` must exit 0. stderr={stderr}");
    assert!(
        accounts_of(&config).is_empty(),
        "nothing may be written when there is no login to import"
    );
    assert!(
        stderr.contains("no accounts configured — run `tcr login` to add one"),
        "the empty-fleet hint must print when no login was found: {stderr}"
    );
}

/// A credentials document that exists and cannot be read is ONE warning line,
/// not a non-zero exit — TcrBar renders any non-zero `tcr status` as a failed
/// poll.
#[test]
fn an_unreadable_credentials_document_warns_and_carries_on() {
    let (home, credentials) = scratch_home();
    std::fs::write(&credentials, "{not json at all").expect("writing a corrupt document");

    let (code, _, stderr) = run_tcr(home.path(), &credentials, &["accounts"]);
    assert_eq!(
        code,
        Some(0),
        "an unreadable credential store must not fail the verb. stderr={stderr}"
    );
    assert!(
        stderr.contains("warning") && stderr.contains("Claude Code login"),
        "it must warn once, naming what it could not read: {stderr}"
    );
    assert!(
        stderr.contains("no accounts configured — run `tcr login` to add one"),
        "and still print the ordinary empty-fleet hint: {stderr}"
    );
}

/// [`run_tcr`]'s command, with codex's own login pinned inside the scratch `HOME`: `CODEX_HOME`
/// and `TCR_CODEX_AUTH` removed, so the only codex login the child can find is
/// `<home>/.codex/auth.json`.
fn run_tcr_without_codex(
    home: &Path,
    credentials: &Path,
    args: &[&str],
) -> (Option<i32>, String, String) {
    let bin = env!("CARGO_BIN_EXE_tcr");
    let out = Command::new(bin)
        .args(args)
        .env("HOME", home)
        .env("TCR_CLAUDE_CODE_CREDENTIALS", credentials)
        .env_remove("XDG_CACHE_HOME")
        .env_remove("CODEX_HOME")
        .env_remove("TCR_CODEX_AUTH")
        .output()
        .unwrap_or_else(|err| panic!("spawning the built tcr binary ({bin}) failed: {err}"));
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Puts a file's mode back to `0o600` when dropped, so a test that made it unreadable leaves
/// nothing behind that its tempdir cannot clean up, whether its assertions pass or not.
struct RestoreMode<'a>(&'a Path);

impl Drop for RestoreMode<'_> {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt as _;
        let _restored = std::fs::set_permissions(self.0, std::fs::Permissions::from_mode(0o600));
    }
}

/// `tcr run --codex` on a first run launches codex and nothing else: the Claude Code login on
/// the machine stays where it is, because a Codex session has no use for a Claude account and
/// an import would spend its single-use refresh token. Nothing listens on the config's port,
/// so codex launches directly, as the fake binary here.
///
/// The positive control is the same home's `tcr accounts`, which does import the login: the
/// fake login was importable all along, so the empty fleet after `run --codex` is the verb's
/// choice and not a login it could not read.
#[test]
fn run_codex_on_a_first_run_never_imports_the_claude_login() {
    let (home, credentials) = scratch_home();
    let config = config_path_in(home.path());

    let bin = env!("CARGO_BIN_EXE_tcr");
    let out = Command::new(bin)
        .args(["run", "--codex"])
        .env("HOME", home.path())
        .env("TCR_CLAUDE_CODE_CREDENTIALS", &credentials)
        .env_remove("XDG_CACHE_HOME")
        .env("TCR_CODEX_BIN", "/usr/bin/true")
        .env_remove("CODEX_BIN")
        .env_remove("HTTPS_PROXY")
        .env_remove("https_proxy")
        .output()
        .unwrap_or_else(|err| panic!("spawning the built tcr binary ({bin}) failed: {err}"));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(0),
        "codex launched directly and exited 0. stderr={stderr}"
    );
    assert!(
        accounts_of(&config).is_empty(),
        "`tcr run --codex` must never import the Claude Code login. stderr={stderr}"
    );

    let (code, _, stderr) = run_tcr(home.path(), &credentials, &["accounts"]);
    assert_eq!(code, Some(0), "stderr={stderr}");
    assert_eq!(
        accounts_of(&config).len(),
        1,
        "control: `tcr accounts` imports the same login. stderr={stderr}"
    );
}

/// A fleet a delete emptied (`"claudeCodeImport": false`) imports no Claude Code login, and
/// still names a codex login on disk: the key stops the import and nothing else, and the codex
/// line is evidence, never an import. The login file is checked for existence only, and left
/// as it was.
///
/// The positive control is the same home with the key removed, which imports the login: the
/// fake login was importable all along.
#[test]
fn a_fleet_a_delete_emptied_keeps_the_codex_hint_and_imports_nothing() {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let (home, credentials) = scratch_home();
    let config = config_path_in(home.path());
    let set_import_key = |value: Option<bool>| {
        let mut document: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config).expect("the config"))
                .expect("a readable config");
        let object = document.as_object_mut().expect("the config is an object");
        match value {
            Some(value) => object.insert("claudeCodeImport".to_string(), value.into()),
            None => object.remove("claudeCodeImport"),
        };
        std::fs::write(&config, document.to_string()).expect("writing the config back");
    };
    set_import_key(Some(false));

    let login = home.path().join(".codex").join("auth.json");
    std::fs::create_dir_all(login.parent().expect("the login has a parent"))
        .expect("creating the scratch .codex dir");
    std::fs::write(&login, "").expect("writing an empty login file");
    std::fs::set_permissions(&login, std::fs::Permissions::from_mode(0o000))
        .expect("making the login unreadable");
    let _restore = RestoreMode(&login);

    let (code, _, stderr) = run_tcr_without_codex(home.path(), &credentials, &["accounts"]);
    assert_eq!(code, Some(0), "stderr={stderr}");
    let stderr = stderr.replace(&home.path().display().to_string(), "<home>");
    let lines: Vec<&str> = stderr.lines().collect();
    let empty_fleet = lines
        .iter()
        .position(|line| *line == "no accounts configured — run `tcr login` to add one")
        .unwrap_or_else(|| panic!("no empty-fleet line: {stderr}"));
    assert_eq!(
        lines.get(empty_fleet + 1).copied(),
        Some(
            "[tcr] found a codex login at <home>/.codex/auth.json: `tcr login --codex` adds a \
             Codex account of tcr's own, and `tcr login --from-codex` imports that one with \
             guarded token updates shared through auth.json"
        ),
        "{stderr}"
    );
    assert!(
        !lines.iter().any(|line| line.starts_with("[tcr] imported")),
        "{stderr}"
    );
    assert!(accounts_of(&config).is_empty(), "{stderr}");
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&config).expect("the config"))
            .expect("a readable config");
    assert_eq!(written["claudeCodeImport"], serde_json::json!(false));
    let after = std::fs::metadata(&login).expect("the login is still there");
    assert_eq!(after.len(), 0, "the login file is untouched");
    assert_eq!(after.mode() & 0o777, 0o000, "the login file keeps its mode");

    set_import_key(None);
    let (code, _, stderr) = run_tcr(home.path(), &credentials, &["accounts"]);
    assert_eq!(code, Some(0), "stderr={stderr}");
    assert_eq!(
        accounts_of(&config).len(),
        1,
        "control: without the key the same login imports. stderr={stderr}"
    );
}
