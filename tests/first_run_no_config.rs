//! Proves first-run file creation through status and offline accounts, plus status
//! output through an existing empty config on an isolated port. Fresh installs use `~/.config/tcr.json`;
//! an install that already has `~/.config/teamclaude.json` keeps using it.
//!
//! This is an end-to-end test of the BUILT BINARY rather than a unit test of
//! `config::load_or_init`, because the defect it guards was never in the load
//! function — it was in which load function each verb reached for. `tcr status`
//! called `config::load`, got `Io(NotFound)`, and exited 1 with `config i/o
//! error: No such file or directory`; TcrBar's panel shells to `tcr status
//! --json` and renders a non-zero exit as a failed poll, so a fresh install off
//! the dmg showed an error in the menu bar while the proxy behind it was up. A
//! unit test on the config module would have stayed green through all of that.
//!
//! An explicit missing config takes the first-empty-fleet offline path before any
//! port probe. Existing-config status fixtures name an isolated port.
//!
//! # Isolation
//!
//! Same rules as `tests/headless_sigterm.rs`: `HOME` points at a fresh
//! [`tempfile::TempDir`] and `XDG_CACHE_HOME` is removed, both set on the
//! spawned [`Command`] rather than on this test process, so the real
//! `~/.config/teamclaude.json` — which holds working credentials — and the live
//! proxy reading it are never touched. A test-only libc guard rejects port 3456 and
//! non-loopback connections before they leave the child, and records attempts without
//! account data. Its positive control uses only an owned ephemeral listener. Status
//! with an explicit fixture port falls back to its offline snapshot.
use crate::connect_guard;

use std::path::PathBuf;
use std::process::Command;

/// The config path the child will resolve from its own `HOME`
/// (`config::default_path`).
fn config_path_in(home: &std::path::Path) -> PathBuf {
    home.join(".config").join(tcr::paths::CONFIG_FILE)
}

/// The config an install from before the rename has.
fn legacy_config_path_in(home: &std::path::Path) -> PathBuf {
    home.join(".config").join(tcr::legacy_names::CONFIG_FILE)
}

/// Run `tcr <args…>` against a scratch `HOME`, returning (exit code, stdout, stderr).
fn run_tcr(home: &std::path::Path, args: &[&str]) -> (Option<i32>, String, String) {
    if args.first() == Some(&"status") {
        let explicit = args
            .windows(2)
            .find_map(|pair| (pair[0] == "--config").then(|| PathBuf::from(pair[1])));
        let path = explicit.clone().unwrap_or_else(|| config_path_in(home));
        if path.exists() {
            let config: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            let port = config["proxy"]["port"]
                .as_u64()
                .expect("an existing status fixture configures its port");
            assert_ne!(port, 3456, "never contact the live proxy");
        } else {
            assert!(
                explicit.is_some(),
                "missing-file status must name its isolated config explicitly"
            );
            assert!(
                path.starts_with(home),
                "the new file belongs to this fixture's HOME"
            );
        }
    }
    let mut command = crate::tcr_env::tcr(home);
    command
        .args(args)
        .env("ANTHROPIC_BASE_URL", "https://api.anthropic.com")
        .env("CODEX_HOME", home.join(".codex"))
        .env("TCR_CODEX_AUTH", home.join("no-such-codex-auth.json"));
    // The helper's Claude Code login is a file that does not exist: the first run these
    // assertions are about. `tests/claude_code_import.rs` covers the other first run, the one
    // that finds a login.
    let guard = connect_guard::ConnectGuard::attach(&mut command, home);
    let out = command
        .output()
        .unwrap_or_else(|err| panic!("spawning the built tcr binary failed: {err}"));
    guard.assert_no_connections();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Mutation note: point any verb's load back at `config::load` (i.e. undo
/// `cli::load_config`) and this fails on its first assertion with `exit=1` and
/// the `config i/o error: No such file or directory` text in stderr — observed
/// on `346ea60` before the fix, not hypothesised.
#[test]
fn status_with_an_empty_config_exits_zero_offline() {
    let home = tempfile::tempdir().expect("a scratch HOME must be creatable");
    let config = config_path_in(home.path());
    std::fs::create_dir_all(config.parent().expect("the config's parent"))
        .expect("create the fixture config directory");
    let before = b"{\"proxy\":{\"port\":9},\"accounts\":[]}";
    std::fs::write(&config, before).expect("write the isolated empty config");

    let (code, stdout, stderr) = run_tcr(home.path(), &["status", "--json"]);
    assert_eq!(
        code,
        Some(0),
        "`tcr status --json` on an empty fleet must exit 0 (TcrBar renders any other \
         exit as a failed poll). stderr={stderr}"
    );

    // A read-only status must retain the explicit fixture port and the config bytes.
    let written = std::fs::read_to_string(&config)
        .unwrap_or_else(|err| panic!("no config at {} after the run: {err}", config.display()));
    let parsed: serde_json::Value =
        serde_json::from_str(&written).unwrap_or_else(|err| panic!("unparseable config: {err}"));
    assert_eq!(written.as_bytes(), before);
    assert_eq!(parsed["proxy"]["port"], 9);
    assert_eq!(
        parsed.get("accounts").and_then(|a| a.as_array()),
        Some(&vec![]),
        "a created config must hold an empty accounts array, got {written}"
    );

    // The status read is offline and this fixture has no accounts, so the array is empty.
    let fleet: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|_| panic!("`status --json` stdout is not a JSON document"));
    assert_eq!(fleet, serde_json::json!([]));
    assert!(
        stderr.contains("no accounts configured — run `tcr login` to add one"),
        "the empty-fleet hint must be on stderr, got: {stderr}"
    );
}

/// Status itself initializes a missing file, not only the offline accounts verb.
/// The creation-aware status path never probes a default port for this empty first run.
#[test]
fn status_with_an_explicit_missing_config_creates_an_empty_offline_fleet() {
    let home = tempfile::tempdir().unwrap();
    let config = config_path_in(home.path());
    assert!(!config.exists());
    let (code, stdout, stderr) = run_tcr(
        home.path(),
        &["status", "--json", "--config", config.to_str().unwrap()],
    );
    assert_eq!(
        code,
        Some(0),
        "a first status read must initialize the file: {stderr}"
    );
    let document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(config).unwrap()).unwrap();
    assert_eq!(document["accounts"], serde_json::json!([]));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(stdout.trim()).unwrap(),
        serde_json::json!([])
    );
}

#[test]
fn a_first_run_text_status_still_explains_claudes_route() {
    let home = tempfile::tempdir().unwrap();
    let config = config_path_in(home.path());
    let (code, stdout, _) = run_tcr(
        home.path(),
        &["status", "--config", config.to_str().unwrap()],
    );
    assert_eq!(code, Some(0));
    assert!(
        stdout.contains("Claude") && stdout.contains("verdict:"),
        "first-run text status retains the environment-only Claude route verdict"
    );
}

/// An install from before the rename keeps its config where it is: the binary
/// reads `teamclaude.json`, and creates no `tcr.json` beside it, which would
/// silently take over on the next run with an empty account list.
#[test]
fn an_existing_legacy_config_is_used_in_place() {
    let home = tempfile::tempdir().expect("a scratch HOME must be creatable");
    let legacy = legacy_config_path_in(home.path());
    std::fs::create_dir_all(legacy.parent().expect("a parent")).expect("mkdir .config");
    let body = r#"{"accounts":[{"name":"alice@example.com","accessToken":"at-fake"}]}"#;
    std::fs::write(&legacy, body).expect("write the legacy config");

    let (code, stdout, stderr) = run_tcr(home.path(), &["accounts"]);
    assert_eq!(code, Some(0), "stdout={stdout} stderr={stderr}");
    assert!(
        stdout.contains("alice@example.com"),
        "the account in the legacy config must be listed: {stdout}"
    );
    assert!(
        !stderr.contains("created"),
        "an existing legacy config is not a first run: {stderr}"
    );
    assert!(
        !config_path_in(home.path()).exists(),
        "no new-name config may appear beside the legacy one"
    );
}

/// `tcr accounts` is the offline half of the same first run: no port probe, so
/// its empty table and its exit code are this tempdir's alone.
#[test]
fn accounts_with_no_config_exits_zero_with_an_empty_table() {
    let home = tempfile::tempdir().expect("a scratch HOME must be creatable");
    let (code, stdout, stderr) = run_tcr(home.path(), &["accounts"]);
    assert_eq!(
        code,
        Some(0),
        "`tcr accounts` on a fresh HOME must exit 0. stdout={stdout} stderr={stderr}"
    );
    assert!(
        !stdout.contains("Error"),
        "stdout must stay the ordinary (empty) table: {stdout}"
    );
    assert!(
        stderr.contains("no accounts configured — run `tcr login` to add one"),
        "the empty-fleet hint must be on stderr, got: {stderr}"
    );
    assert!(
        config_path_in(home.path()).exists(),
        "`tcr accounts` must create the config it could not find"
    );
    assert!(
        stderr.contains("created") && stderr.contains(tcr::paths::CONFIG_FILE),
        "creating the config must say so once, got: {stderr}"
    );
    assert!(
        !legacy_config_path_in(home.path()).exists(),
        "a fresh install gets only the new name"
    );
}

/// The second run must be an ordinary run: no second "created" line, still
/// exit 0, and the file it wrote the first time is what it reads back.
#[test]
fn the_second_run_reuses_the_file_it_created() {
    let home = tempfile::tempdir().expect("a scratch HOME must be creatable");
    let (first, _, _) = run_tcr(home.path(), &["accounts"]);
    assert_eq!(first, Some(0), "first `tcr accounts` must exit 0");
    let after_first = std::fs::read(config_path_in(home.path())).expect("the config was created");

    let (second, _, stderr) = run_tcr(home.path(), &["accounts"]);
    assert_eq!(second, Some(0), "second `tcr accounts` must exit 0");
    assert!(
        !stderr.contains("created"),
        "an existing config must not be reported as created: {stderr}"
    );
    assert_eq!(
        std::fs::read(config_path_in(home.path())).expect("the config is still there"),
        after_first,
        "a read-only verb must leave the file byte-for-byte alone"
    );
}

/// [`run_tcr`]'s command, with codex's own login pinned inside the scratch `HOME`: `CODEX_HOME`
/// and `TCR_CODEX_AUTH` are removed, so the only codex login the child can find is
/// `<home>/.codex/auth.json`. A developer's mac usually has a real one at `~/.codex`, and a
/// first run that found it would print a line these assertions are not about.
fn command_without_codex(home: &std::path::Path, args: &[&str]) -> Command {
    let mut command = crate::tcr_env::tcr(home);
    command.args(args);
    command
}

/// Run `command`, returning (exit code, stdout, stderr).
fn outcome(command: &mut Command) -> (Option<i32>, String, String) {
    let out = command
        .output()
        .unwrap_or_else(|err| panic!("spawning the built tcr binary failed: {err}"));
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// `tcr <args…>` through [`command_without_codex`].
fn run_tcr_without_codex(home: &std::path::Path, args: &[&str]) -> (Option<i32>, String, String) {
    outcome(&mut command_without_codex(home, args))
}

/// `text` with the scratch HOME, as written and in its canonical form, replaced by `<home>`.
fn normalize_home(text: &str, home: &std::path::Path) -> String {
    let mut out = text.to_string();
    if let Ok(canonical) = std::fs::canonicalize(home) {
        out = out.replace(&canonical.display().to_string(), "<home>");
    }
    out.replace(&home.display().to_string(), "<home>")
}

/// What a first run prints with no codex login on the machine, pinned before the empty-fleet
/// hint learned about codex logins: with no evidence of codex, the lines are exactly the ones a
/// Claude-only first run always printed. Twice, each on a fresh HOME: once with no
/// `CODEX_HOME`, and once with `CODEX_HOME` naming an empty folder.
#[test]
fn an_empty_fleet_without_a_codex_login_prints_todays_lines() {
    const STDERR: &str = "[tcr] created <home>/.config/tcr.json with no accounts\n\
                          no accounts configured — run `tcr login` to add one\n";

    let home = tempfile::tempdir().expect("a scratch HOME must be creatable");
    let (code, stdout, stderr) = run_tcr_without_codex(home.path(), &["accounts"]);
    assert_eq!(code, Some(0), "stderr={stderr}");
    assert_eq!(stdout, "no accounts configured\n");
    assert_eq!(normalize_home(&stderr, home.path()), STDERR);

    let home = tempfile::tempdir().expect("a scratch HOME must be creatable");
    let empty_codex_home = tempfile::tempdir().expect("an empty CODEX_HOME must be creatable");
    let (code, stdout, stderr) = outcome(
        command_without_codex(home.path(), &["accounts"])
            .env("CODEX_HOME", empty_codex_home.path()),
    );
    assert_eq!(code, Some(0), "stderr={stderr}");
    assert_eq!(stdout, "no accounts configured\n");
    assert_eq!(normalize_home(&stderr, home.path()), STDERR);
}

/// Puts a file's mode back to `0o600` when dropped, so a test that made it unreadable leaves
/// nothing behind that its tempdir cannot clean up, whether its assertions pass or not.
struct RestoreMode<'a>(&'a std::path::Path);

impl Drop for RestoreMode<'_> {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt as _;
        let _restored = std::fs::set_permissions(self.0, std::fs::Permissions::from_mode(0o600));
    }
}

/// A first run on a machine where codex holds a login prints one more stderr line, after the
/// empty-fleet line: the two ways to pool codex. The login is evidence and nothing more: it is
/// checked for existence, never opened (the file here is unreadable to everyone, and still
/// counts), never imported, and left exactly as it was.
#[test]
fn a_codex_login_on_disk_adds_one_hint_line() {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let home = tempfile::tempdir().expect("a scratch HOME must be creatable");
    let login = home.path().join(".codex").join("auth.json");
    std::fs::create_dir_all(login.parent().expect("the login has a parent"))
        .expect("creating the scratch .codex dir");
    std::fs::write(&login, "").expect("writing an empty login file");
    std::fs::set_permissions(&login, std::fs::Permissions::from_mode(0o000))
        .expect("making the login unreadable");
    let _restore = RestoreMode(&login);

    let (code, stdout, stderr) = run_tcr_without_codex(home.path(), &["accounts"]);
    assert_eq!(code, Some(0), "stderr={stderr}");
    assert_eq!(stdout, "no accounts configured\n");
    assert_eq!(
        normalize_home(&stderr, home.path()),
        "[tcr] created <home>/.config/tcr.json with no accounts\n\
         no accounts configured — run `tcr login` to add one\n\
         [tcr] found a codex login at <home>/.codex/auth.json: `tcr login --codex` adds a Codex \
         account of tcr's own, and `tcr login --from-codex` imports that one with guarded \
         token updates shared through auth.json\n"
    );

    let after = std::fs::metadata(&login).expect("the login is still there");
    assert_eq!(after.len(), 0, "the login file is untouched");
    assert_eq!(after.mode() & 0o777, 0o000, "the login file keeps its mode");
    let written = std::fs::read_to_string(config_path_in(home.path())).expect("the config");
    let parsed: serde_json::Value = serde_json::from_str(&written).expect("a readable config");
    assert_eq!(
        parsed.get("accounts").and_then(|a| a.as_array()),
        Some(&vec![]),
        "nothing is imported from a codex login: {written}"
    );
}
