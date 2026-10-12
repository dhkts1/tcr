//! `tcr login --from-codex`, end to end against the BUILT BINARY: the flag is wired to the
//! import, the import reads the file it is pointed at, and the row it writes is a Codex row named
//! by the login's email, or `<email>/codex` beside an Anthropic row that holds that email.
//!
//! # Isolation
//!
//! Pinned on the spawned [`Command`], never on this test process (whose tests run on parallel
//! threads that share one environment):
//!
//! * `HOME` → a fresh [`tempfile::TempDir`], so the real `~/.config/teamclaude.json` (working
//!   credentials for real accounts) and the real `~/.codex` are never read or written.
//! * `TCR_CODEX_AUTH` → a fake `auth.json` inside that tempdir, or a path that does not exist. It
//!   wins over every other place a Codex login is looked for, so the developer's REAL Codex login
//!   can never be imported into the config under test. `CODEX_HOME` is removed for the same
//!   reason. The tests that read through `--codex-home`, `CODEX_HOME` or the default
//!   `$HOME/.codex` remove `TCR_CODEX_AUTH` instead, and every directory they name — `$HOME`
//!   included — is inside the tempdir.
//! * `TCR_CLAUDE_CODE_CREDENTIALS` → a path that does not exist, so nothing in the run reads the
//!   developer's Claude Code login out of the login Keychain either (a scratch `HOME` does not
//!   scratch the Keychain).
//! * `proxy.port` in the config the test writes → a port nothing is listening on, obtained by
//!   binding and releasing one. The import probes that port for a live account-add route before it
//!   writes (`oauth::login_route`), and this machine may well have a real proxy on the default
//!   3456, which these tests must neither talk to nor add an account to.
//! * `codexUpstream` in the config the test writes → a stub on loopback ([`spawn_codex_upstream`]).
//!   Before it writes, the import asks `codexUpstream` where the login's workspace is served
//!   (`codex_login::check_workspace_routing`), and the default is the real chatgpt.com.
//!
//! The fixture's JWTs are put together at run time from a header and a payload, as the unit tests
//! in `src/codex_oauth.rs` put theirs together: the dotted form written out whole is what a secret
//! scanner's JWT rule matches.
use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// `{"alg":"none","typ":"JWT"}`.
const JWT_HEADER: &str = "eyJhbGciOiJub25lIiwidHlwIjoiSldUIn0";
/// `{"exp":1800000000,"https://api.openai.com/auth":{"chatgpt_account_id":
/// "11111111-1111-4111-8111-111111111111","chatgpt_plan_type":"pro"}}`.
const ACCESS_PAYLOAD: &str = "eyJleHAiOjE4MDAwMDAwMDAsImh0dHBzOi8vYXBpLm9wZW5haS5jb20vYXV0aCI6eyJjaGF0Z3B0X2FjY291bnRfaWQiOiIxMTExMTExMS0xMTExLTQxMTEtODExMS0xMTExMTExMTExMTEiLCJjaGF0Z3B0X3BsYW5fdHlwZSI6InBybyJ9fQ";
/// The same account, plus `"email":"alice@example.com"` and a `chatgpt_user_id`.
const ID_PAYLOAD: &str = "eyJleHAiOjE4MDAwMDAwMDAsImVtYWlsIjoiYWxpY2VAZXhhbXBsZS5jb20iLCJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiMTExMTExMTEtMTExMS00MTExLTgxMTEtMTExMTExMTExMTExIiwiY2hhdGdwdF9wbGFuX3R5cGUiOiJwcm8iLCJjaGF0Z3B0X3VzZXJfaWQiOiJ1c2VyLWZha2UifX0";

/// The `auth.json` a ChatGPT login leaves behind, with the fake values `codex_login`'s unit tests
/// use, and the key names Codex writes.
fn fake_codex_auth_json() -> String {
    format!(
        r#"{{"auth_mode":"chatgpt","OPENAI_API_KEY":null,"tokens":{{"id_token":"{JWT_HEADER}.{ID_PAYLOAD}.sig","access_token":"{JWT_HEADER}.{ACCESS_PAYLOAD}.sig","refresh_token":"rt-fake-codex","account_id":"11111111-1111-4111-8111-111111111111"}},"last_refresh":"2026-10-03T06:44:00.000000Z"}}"#
    )
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

/// An accounts/check answer that clears the fixture login's workspace for the pool: served at
/// chatgpt.com, with no routing override.
const UNCONSTRAINED_WORKSPACE: &str = r#"{"accounts":{"11111111-1111-4111-8111-111111111111":{"workspace_backend_origin":"NO_CONSTRAINT","account_routing_override":"NO_CONSTRAINT","plan_type":"pro"}},"account_ordering":["11111111-1111-4111-8111-111111111111"]}"#;

/// A stub `codexUpstream` on `127.0.0.1:0` that answers every request `200` with `answer`, on a
/// thread of its own for the rest of the test process. Returns its base URL and how many
/// requests it has answered.
fn spawn_codex_upstream(answer: &'static str) -> (String, Arc<AtomicUsize>) {
    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").expect("binding the stub upstream must work");
    let addr = listener
        .local_addr()
        .expect("a bound listener has an address");
    let answered = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&answered);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            // Every request the import sends is a GET with no body: read its head, then answer.
            let mut reader = BufReader::new(&mut stream);
            let mut line = String::new();
            while reader.read_line(&mut line).is_ok_and(|n| n > 0) && line != "\r\n" {
                line.clear();
            }
            counter.fetch_add(1, Ordering::SeqCst);
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                 connection: close\r\n\r\n{answer}",
                answer.len()
            );
        }
    });
    (format!("http://{addr}"), answered)
}

/// A scratch `HOME` holding a config on a dead port whose `accounts` array is `accounts` (JSON)
/// and whose `codexUpstream` clears the fixture login's workspace. Returns the tempdir and the
/// config's path.
fn scratch_home(accounts: &str) -> (tempfile::TempDir, PathBuf) {
    let (home, config, _answered) = scratch_home_answering(accounts, UNCONSTRAINED_WORKSPACE);
    (home, config)
}

/// [`scratch_home`], with a `codexUpstream` that answers the workspace check with `answer`; also
/// returns how many requests that upstream has answered.
fn scratch_home_answering(
    accounts: &str,
    answer: &'static str,
) -> (tempfile::TempDir, PathBuf, Arc<AtomicUsize>) {
    let home = tempfile::tempdir().expect("a scratch HOME must be creatable");
    let config = home.path().join(".config").join("teamclaude.json");
    std::fs::create_dir_all(config.parent().expect("the config has a parent"))
        .expect("creating the scratch .config dir");
    let (upstream, answered) = spawn_codex_upstream(answer);
    std::fs::write(
        &config,
        format!(
            r#"{{ "proxy": {{ "port": {} }}, "codexUpstream": "{upstream}", "accounts": {accounts} }}"#,
            dead_port()
        ),
    )
    .expect("writing the scratch config");
    (home, config, answered)
}

/// Run `tcr login --from-codex --config <config> <extra…>` against the scratch `HOME`, with the
/// credential stores pinned as the module doc says. `auth` is offered as `TCR_CODEX_AUTH`; `None`
/// removes it, so the login can only come from a `--codex-home` in `extra` or the scratch `HOME`.
fn login_from_codex(home: &Path, config: &Path, auth: Option<&Path>, extra: &[&str]) -> Output {
    let bin = env!("CARGO_BIN_EXE_tcr");
    codex_command(home, config, auth, extra)
        .output()
        .unwrap_or_else(|err| panic!("spawning the built tcr binary ({bin}) failed: {err}"))
}

/// The [`Command`] [`login_from_codex`] runs, for a test that has one more variable to set on it.
fn codex_command(home: &Path, config: &Path, auth: Option<&Path>, extra: &[&str]) -> Command {
    let bin = env!("CARGO_BIN_EXE_tcr");
    let mut command = Command::new(bin);
    command
        .args(["login", "--from-codex", "--config"])
        .arg(config)
        .args(extra)
        .env("HOME", home)
        .env(
            "TCR_CLAUDE_CODE_CREDENTIALS",
            home.join("no-claude-code-login.json"),
        )
        .env_remove("CODEX_HOME")
        .env_remove("XDG_CACHE_HOME");
    match auth {
        Some(path) => command.env("TCR_CODEX_AUTH", path),
        None => command.env_remove("TCR_CODEX_AUTH"),
    };
    command
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

/// No output may carry token material: not the refresh token, and neither the access token's
/// payload nor the id token's.
fn assert_no_token_in(out: &Output) {
    for stream in [&out.stdout, &out.stderr] {
        let text = String::from_utf8_lossy(stream);
        assert!(
            !text.contains("rt-fake-codex")
                && !text.contains(ACCESS_PAYLOAD)
                && !text.contains(ID_PAYLOAD),
            "no token on stdout or stderr, ever: {text}"
        );
    }
}

#[test]
fn from_codex_imports_a_codex_row_with_provider_identity_and_plan() {
    let (home, config) = scratch_home("[]");
    let auth = home.path().join("codex-auth.json");
    std::fs::write(&auth, fake_codex_auth_json()).expect("writing the fake auth.json");

    let out = login_from_codex(home.path(), &config, Some(&auth), &[]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stderr: {stderr}");

    let accounts = accounts_of(&config);
    assert_eq!(accounts.len(), 1, "exactly the imported row: {accounts:?}");
    let row = &accounts[0];
    assert_eq!(row["name"], "alice@example.com");
    assert_eq!(row["provider"], "codex");
    assert_eq!(row["accountUuid"], "11111111-1111-4111-8111-111111111111");
    assert_eq!(row["organizationType"], "pro");
    assert_eq!(row["expiresAt"], 1_800_000_000_000_i64);
    assert_eq!(row["refreshToken"], "rt-fake-codex");
    assert!(
        stderr.contains("single-use"),
        "the refresh-token warning must print on every import: {stderr}"
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("Logged in as 'alice@example.com'."),
        "stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_no_token_in(&out);
}

#[test]
fn from_codex_beside_an_anthropic_row_of_the_same_email_adds_a_suffixed_row() {
    let (home, config) =
        scratch_home(r#"[{ "name": "alice@example.com", "accessToken": "at-a" }]"#);
    let auth = home.path().join("codex-auth.json");
    std::fs::write(&auth, fake_codex_auth_json()).expect("writing the fake auth.json");

    let out = login_from_codex(home.path(), &config, Some(&auth), &[]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let accounts = accounts_of(&config);
    assert_eq!(
        accounts.len(),
        2,
        "the import lands BESIDE the Anthropic row: {accounts:?}"
    );
    let anthropic = &accounts[0];
    assert_eq!(anthropic["name"], "alice@example.com");
    assert_eq!(
        anthropic["accessToken"], "at-a",
        "the Anthropic row keeps its own credential"
    );
    assert!(
        anthropic.get("provider").is_none() && anthropic.get("refreshToken").is_none(),
        "the Anthropic row is untouched: {anthropic}"
    );
    let codex = &accounts[1];
    assert_eq!(codex["name"], "alice@example.com/codex");
    assert_eq!(codex["provider"], "codex");
    assert_eq!(codex["refreshToken"], "rt-fake-codex");
    assert_no_token_in(&out);
}

#[test]
fn from_codex_with_no_login_writes_nothing_and_fails_clearly() {
    let (home, config) = scratch_home("[]");
    let before = std::fs::read(&config).expect("the scratch config is there");

    let absent = home.path().join("no-codex-login.json");
    let out = login_from_codex(home.path(), &config, Some(&absent), &[]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "no login to import must exit non-zero: {stderr}"
    );
    assert!(
        stderr.contains("no Codex login"),
        "the failure must say what is missing: {stderr}"
    );
    assert_eq!(
        std::fs::read(&config).expect("the scratch config is still there"),
        before,
        "nothing may be written when there is no login to import"
    );
    assert_no_token_in(&out);
}

/// A login whose ChatGPT workspace the check places anywhere but chatgpt.com is refused through
/// the binary, naming what the check said, after asking its stub `codexUpstream` once; the
/// config is left as it was.
#[test]
fn from_codex_refuses_a_workspace_served_elsewhere_and_writes_nothing() {
    const ROUTED: &str = r#"{"accounts":{"11111111-1111-4111-8111-111111111111":{"workspace_backend_origin":"https://gov.chatgpt.com","account_routing_override":"NO_CONSTRAINT"}}}"#;
    let (home, config, answered) = scratch_home_answering("[]", ROUTED);
    let auth = home.path().join("codex-auth.json");
    std::fs::write(&auth, fake_codex_auth_json()).expect("writing the fake auth.json");
    let before = std::fs::read(&config).expect("the scratch config is there");

    let out = login_from_codex(home.path(), &config, Some(&auth), &[]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "a routed workspace must fail: {stderr}"
    );
    assert!(
        stderr.contains("not served at https://chatgpt.com")
            && stderr.contains("https://gov.chatgpt.com")
            && stderr.contains("Nothing was written"),
        "stderr: {stderr}"
    );
    assert_eq!(answered.load(Ordering::SeqCst), 1, "one workspace check");
    assert_eq!(
        std::fs::read(&config).expect("the scratch config is still there"),
        before,
        "nothing may be written"
    );
    assert_no_token_in(&out);
}

/// A FedRAMP login (its id token carries `chatgpt_account_is_fedramp: true`) is refused through
/// the binary before the import asks anything of anyone: its stub `codexUpstream` answers no
/// request, and the config is left as it was.
#[test]
fn from_codex_refuses_a_fedramp_login_without_asking_anyone() {
    /// The fixture id token's payload with `"chatgpt_plan_type":"enterprise"` and
    /// `"chatgpt_account_is_fedramp":true` under `https://api.openai.com/auth`.
    const FEDRAMP_ID_PAYLOAD: &str = "eyJleHAiOjE4MDAwMDAwMDAsImVtYWlsIjoiYWxpY2VAZXhhbXBsZS5jb20iLCJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiMTExMTExMTEtMTExMS00MTExLTgxMTEtMTExMTExMTExMTExIiwiY2hhdGdwdF9wbGFuX3R5cGUiOiJlbnRlcnByaXNlIiwiY2hhdGdwdF9hY2NvdW50X2lzX2ZlZHJhbXAiOnRydWV9fQ";
    let (home, config, answered) = scratch_home_answering("[]", UNCONSTRAINED_WORKSPACE);
    let auth = home.path().join("codex-auth.json");
    std::fs::write(
        &auth,
        fake_codex_auth_json().replace(ID_PAYLOAD, FEDRAMP_ID_PAYLOAD),
    )
    .expect("writing the fake auth.json");
    let before = std::fs::read(&config).expect("the scratch config is there");

    let out = login_from_codex(home.path(), &config, Some(&auth), &[]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "a FedRAMP login must fail: {stderr}");
    assert!(
        stderr.contains("FedRAMP") && stderr.contains("Nothing was written"),
        "stderr: {stderr}"
    );
    assert_eq!(answered.load(Ordering::SeqCst), 0, "nothing was asked");
    assert_eq!(
        std::fs::read(&config).expect("the scratch config is still there"),
        before,
        "nothing may be written"
    );
    assert!(
        !stderr.contains(FEDRAMP_ID_PAYLOAD),
        "no token on stderr: {stderr}"
    );
}

/// `--codex-home <DIR>` reads `<DIR>/auth.json`, the second-account path (`CODEX_HOME=<DIR> codex
/// login`, then this). `TCR_CODEX_AUTH` is removed for this run and `HOME` is still the scratch
/// dir, so the one login the binary can find is the one under `<DIR>`.
#[test]
fn from_codex_with_codex_home_reads_the_login_under_that_dir() {
    let (home, config) = scratch_home("[]");
    let second = home.path().join("second-codex-login");
    std::fs::create_dir_all(&second).expect("creating the scratch codex home");
    std::fs::write(second.join("auth.json"), fake_codex_auth_json())
        .expect("writing the fake auth.json");

    let second_arg = second.to_str().expect("a scratch path is UTF-8");
    let out = login_from_codex(home.path(), &config, None, &["--codex-home", second_arg]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let accounts = accounts_of(&config);
    assert_eq!(accounts.len(), 1, "{accounts:?}");
    assert_eq!(accounts[0]["name"], "alice@example.com");
    assert_eq!(accounts[0]["provider"], "codex");
    assert_no_token_in(&out);
}

/// The place most users' login is read from: `$HOME/.codex/auth.json`, with neither
/// `TCR_CODEX_AUTH` nor `CODEX_HOME` set — read end to end through the binary's own environment
/// wrappers, which the unit tests deliberately never touch. The success line also tells the user
/// what the import costs an older proxy, and that `tcr run --codex` does not spare the copy of the
/// login `codex` holds.
#[test]
fn from_codex_reads_the_login_under_the_default_codex_home() {
    let (home, config) = scratch_home("[]");
    let default_home = home.path().join(".codex");
    std::fs::create_dir_all(&default_home).expect("creating the scratch ~/.codex");
    std::fs::write(default_home.join("auth.json"), fake_codex_auth_json())
        .expect("writing the fake auth.json");

    let out = login_from_codex(home.path(), &config, None, &[]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stderr: {stderr}");
    let accounts = accounts_of(&config);
    assert_eq!(accounts.len(), 1, "{accounts:?}");
    assert_eq!(accounts[0]["name"], "alice@example.com");
    assert_eq!(accounts[0]["provider"], "codex");
    assert!(
        stderr.contains(
            "Update every tcr that may start on this config, TcrBar's bundled one included, \
             before it next starts: an older one refuses to start, or reads this login as a \
             Claude account."
        ),
        "the import must say what it costs an older proxy: {stderr}"
    );
    assert!(
        stderr.contains("under `tcr run --codex` too")
            && stderr.contains(
                "`CODEX_HOME=<dir> codex login`, then `tcr login --from-codex --codex-home <dir>`"
            )
            && !stderr.contains("unaffected"),
        "the import must say `tcr run --codex` does not spare the copy `codex` holds, and how \
         to avoid that: {stderr}"
    );
    assert_no_token_in(&out);
}

/// `CODEX_HOME` moves where the login is read: `$CODEX_HOME/auth.json`, and NOT the one under
/// `$HOME/.codex` — an API-key login here, which would fail the import if it were the one read.
#[test]
fn from_codex_reads_the_login_under_codex_home_from_the_environment() {
    let (home, config) = scratch_home("[]");
    let decoy = home.path().join(".codex");
    std::fs::create_dir_all(&decoy).expect("creating the scratch ~/.codex");
    std::fs::write(
        decoy.join("auth.json"),
        r#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-fake"}"#,
    )
    .expect("writing the decoy auth.json");
    let elsewhere = home.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).expect("creating the scratch CODEX_HOME");
    std::fs::write(elsewhere.join("auth.json"), fake_codex_auth_json())
        .expect("writing the fake auth.json");

    let mut command = codex_command(home.path(), &config, None, &[]);
    command.env("CODEX_HOME", &elsewhere);
    let out = command.output().expect("spawning the built tcr binary");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let accounts = accounts_of(&config);
    assert_eq!(accounts.len(), 1, "{accounts:?}");
    assert_eq!(accounts[0]["name"], "alice@example.com");
    assert_eq!(accounts[0]["provider"], "codex");
    assert_no_token_in(&out);
}
