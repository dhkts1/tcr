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
//! * `proxy.port` in the config the test writes → `0`, which cannot name an actual listener.
//!   The import probes that port for a live account-add route before it writes
//!   (`oauth::login_route`). A released ephemeral port can be reused by another test's stub;
//!   port zero avoids that race and never reaches a real proxy on the default 3456.
//! * `codexUpstream` in the config the test writes → a stub on loopback ([`spawn_codex_upstream`]).
//!   Before it writes, the import asks `codexUpstream` where the login's workspace is served
//!   (`codex_login::check_workspace_routing`), and the default is the real chatgpt.com.
//! * `PATH` and `TCR_CODEX_SEARCH_DIRS` → empty directories inside the tempdir, and
//!   `TCR_BUNDLED_TCR` → a path that does not exist ([`pinned_tcr`]). After it writes, the import
//!   asks every `tcr` it can see for its version, to warn about one that predates Codex accounts,
//!   and `/opt/homebrew/bin`, `/usr/local/bin` and `/Applications` are not under a scratch `HOME`:
//!   without these, a test would run the developer's own `tcr`.
//!
//! The fixture's JWTs are put together at run time from a header and a payload, as the unit tests
//! in `src/codex_oauth.rs` put theirs together: the dotted form written out whole is what a secret
//! scanner's JWT rule matches.
use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tcr::log_err::LogErr as _;

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

/// An accounts/check answer that clears the fixture login's workspace for the pool: served at
/// chatgpt.com, with no routing override.
const UNCONSTRAINED_WORKSPACE: &str = r#"{"accounts":{"11111111-1111-4111-8111-111111111111":{"workspace_backend_origin":"NO_CONSTRAINT","account_routing_override":"NO_CONSTRAINT","plan_type":"pro"}},"account_ordering":["11111111-1111-4111-8111-111111111111"]}"#;

/// A stub `codexUpstream` on `127.0.0.1:0` that answers every request `200` with `answer`, on a
/// thread of its own for the rest of the test process. Returns its base URL and how many
/// requests it has answered.
fn spawn_codex_upstream(answer: &'static str) -> (String, Arc<AtomicUsize>) {
    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").expect("binding the stub upstream must work");
    serve_codex_upstream(listener, answer)
}

fn serve_codex_upstream(
    listener: std::net::TcpListener,
    answer: &'static str,
) -> (String, Arc<AtomicUsize>) {
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
            write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                 connection: close\r\n\r\n{answer}",
                answer.len()
            )
            .log_debug("writing a test response");
        }
    });
    (format!("http://{addr}"), answered)
}

/// A scratch `HOME` holding an offline config whose `accounts` array is `accounts` (JSON)
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
    let config = home.path().join(".config").join(tcr::paths::CONFIG_FILE);
    std::fs::create_dir_all(config.parent().expect("the config has a parent"))
        .expect("creating the scratch .config dir");
    let (upstream, answered) = spawn_codex_upstream(answer);
    std::fs::write(
        &config,
        format!(
            r#"{{ "proxy": {{ "port": 0 }}, "codexUpstream": "{upstream}", "accounts": {accounts} }}"#
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

/// The built `tcr`, pinned as the module doc says: `HOME` the scratch dir; the Claude Code
/// login a file that does not exist (never an empty value, which reads as unset and falls
/// through to the login Keychain); `PATH` and the search directories empty; no bundled `tcr`;
/// and no `CODEX_HOME`, `XDG_CACHE_HOME` or `TCR_CODEX_AUTH`.
fn pinned_tcr(home: &Path) -> Command {
    let empty_path = home.join("empty-path");
    let empty_search = home.join("empty-search");
    for dir in [&empty_path, &empty_search] {
        std::fs::create_dir_all(dir).expect("creating an empty directory in the scratch HOME");
    }
    let mut command = Command::new(env!("CARGO_BIN_EXE_tcr"));
    command
        .env("HOME", home)
        .env(
            "TCR_CLAUDE_CODE_CREDENTIALS",
            home.join("no-claude-code-login.json"),
        )
        .env("PATH", &empty_path)
        .env("TCR_CODEX_SEARCH_DIRS", &empty_search)
        .env("TCR_BUNDLED_TCR", home.join("no-bundled-tcr"))
        .env_remove("CODEX_HOME")
        .env_remove("XDG_CACHE_HOME")
        .env_remove("TCR_CODEX_AUTH");
    command
}

/// The [`Command`] [`login_from_codex`] runs, for a test that has one more variable to set on it.
fn codex_command(home: &Path, config: &Path, auth: Option<&Path>, extra: &[&str]) -> Command {
    let mut command = pinned_tcr(home);
    command
        .args(["login", "--from-codex", "--config"])
        .arg(config)
        .args(extra);
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
    assert_eq!(row["accountUuid"], "user-fake", "the ChatGPT user");
    assert_eq!(
        row["orgUuid"], "11111111-1111-4111-8111-111111111111",
        "the workspace"
    );
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
/// the older-proxy compatibility notice and the guarded token-sharing behavior.
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
        stderr.contains("checks auth.json before writing refreshed tokens")
            && stderr.contains("replacement tokens for the same ChatGPT account")
            && stderr.contains("simultaneous codex refresh can still race this update")
            && stderr.contains(
                "Signing in again with `codex login` in that folder revokes this shared login"
            ),
        "the import must explain guarded token sharing: {stderr}"
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

/// The success line names the plan the way `tcr status` does (`ChatGPT Pro`, through
/// `tcr_status_wire::codex_plan_label`), never as the raw `chatgpt_plan_type` claim (`pro`).
#[test]
fn from_codex_names_the_plan_the_way_status_does() {
    let (home, config) = scratch_home("[]");
    let auth = home.path().join("codex-auth.json");
    std::fs::write(&auth, fake_codex_auth_json()).expect("writing the fake auth.json");

    let out = login_from_codex(home.path(), &config, Some(&auth), &[]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stderr: {stderr}");
    assert!(
        stderr.contains("[tcr] imported as 'alice@example.com' (ChatGPT Pro);"),
        "stderr: {stderr}"
    );
    assert_no_token_in(&out);
}

/// base64url (RFC 4648, section 5), no padding: the encoding a JWT's parts use.
fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, b)| acc | u32::from(*b) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

/// A JWT whose payload is `payload`, put together at run time.
fn jwt(payload: &serde_json::Value) -> String {
    format!(
        "{JWT_HEADER}.{}.sig",
        base64url(payload.to_string().as_bytes())
    )
}

/// The `auth.json` a ChatGPT login of `email`, ChatGPT user `user`, in `workspace` on `plan`
/// leaves behind.
fn auth_json_for(email: &str, user: &str, workspace: &str, plan: &str) -> String {
    let auth = serde_json::json!({
        "chatgpt_account_id": workspace,
        "chatgpt_plan_type": plan,
        "chatgpt_user_id": user,
    });
    serde_json::json!({
        "auth_mode": "chatgpt",
        "OPENAI_API_KEY": null,
        "tokens": {
            "id_token": jwt(&serde_json::json!({
                "exp": 1_800_000_000_i64,
                "email": email,
                "https://api.openai.com/auth": auth,
            })),
            "access_token": jwt(&serde_json::json!({
                "exp": 1_800_000_000_i64,
                "https://api.openai.com/auth": auth,
            })),
            "refresh_token": format!("rt-fake-{email}"),
            "account_id": workspace,
        },
        "last_refresh": "2026-10-03T06:44:00.000000Z",
    })
    .to_string()
}

/// Two seats of one ChatGPT workspace import as two rows: the row is keyed on the ChatGPT user
/// as well as the workspace, so bob's login lands beside alice's instead of over it.
///
/// Watched red: keyed on the workspace alone, bob's import updates alice's row.
#[test]
fn two_seats_of_one_workspace_import_as_two_rows() {
    const WORKSPACE: &str = "66666666-6666-4666-8666-666666666666";
    const SEATS_WORKSPACE: &str = r#"{"accounts":{"66666666-6666-4666-8666-666666666666":{"workspace_backend_origin":"NO_CONSTRAINT","account_routing_override":"NO_CONSTRAINT","plan_type":"business"}}}"#;
    let (home, config, _answered) = scratch_home_answering("[]", SEATS_WORKSPACE);
    for (email, user) in [
        ("alice@example.com", "55555555-5555-4555-8555-555555555555"),
        ("bob@example.com", "77777777-7777-4777-8777-777777777777"),
    ] {
        let auth = home.path().join(format!("{email}.json"));
        std::fs::write(&auth, auth_json_for(email, user, WORKSPACE, "business"))
            .expect("writing the fake auth.json");
        let out = login_from_codex(home.path(), &config, Some(&auth), &[]);
        assert!(
            out.status.success(),
            "{email}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let accounts = accounts_of(&config);
    let seats: Vec<(&str, &str, &str)> = accounts
        .iter()
        .map(|row| {
            (
                row["name"].as_str().unwrap_or_default(),
                row["accountUuid"].as_str().unwrap_or_default(),
                row["orgUuid"].as_str().unwrap_or_default(),
            )
        })
        .collect();
    assert_eq!(
        seats,
        [
            (
                "alice@example.com",
                "55555555-5555-4555-8555-555555555555",
                WORKSPACE
            ),
            (
                "bob@example.com",
                "77777777-7777-4777-8777-777777777777",
                WORKSPACE
            ),
        ]
    );
    assert!(accounts.iter().all(|row| row["provider"] == "codex"));
}

/// An import records the folder its login came from, absolute: `--codex-home <dir>`'s
/// directory, or the directory of the file `TCR_CODEX_AUTH` names.
///
/// Watched red: without the folder passed to the import, neither row has `codexHome`.
#[test]
fn the_import_writes_codex_home() {
    let canonical = |path: &Path| std::fs::canonicalize(path).expect("the folder exists");

    let (home, config) = scratch_home("[]");
    let second = home.path().join("second-codex-login");
    std::fs::create_dir_all(&second).expect("creating the scratch codex home");
    std::fs::write(second.join("auth.json"), fake_codex_auth_json())
        .expect("writing the fake auth.json");
    let mut command = codex_command(
        home.path(),
        &config,
        None,
        &["--codex-home", "second-codex-login"],
    );
    let out = command
        .current_dir(home.path())
        .output()
        .expect("spawning the built tcr binary");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let recorded = accounts_of(&config)[0]["codexHome"]
        .as_str()
        .expect("the row records its folder")
        .to_string();
    assert!(
        Path::new(&recorded).is_absolute(),
        "a relative --codex-home is recorded absolute: {recorded}"
    );
    assert_eq!(canonical(Path::new(&recorded)), canonical(&second));

    let (home, config) = scratch_home("[]");
    let elsewhere = home.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).expect("creating the scratch folder");
    let auth = elsewhere.join("auth.json");
    std::fs::write(&auth, fake_codex_auth_json()).expect("writing the fake auth.json");
    let out = login_from_codex(home.path(), &config, Some(&auth), &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        accounts_of(&config)[0]["codexHome"],
        elsewhere.display().to_string(),
        "the directory of the file TCR_CODEX_AUTH names"
    );
}

#[test]
fn an_override_file_without_codexs_auth_basename_does_not_record_its_parent_for_sync() {
    let (home, config) = scratch_home("[]");
    let auth = home.path().join("standalone-login.json");
    std::fs::write(&auth, fake_codex_auth_json()).unwrap();
    let unrelated = home.path().join("auth.json");
    std::fs::write(&unrelated, "unrelated document").unwrap();
    let out = login_from_codex(home.path(), &config, Some(&auth), &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("without automatic file synchronization"),
        "a standalone override must explain that its source is not synchronized: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        accounts_of(&config)[0].get("codexHome").is_none(),
        "a standalone override must not nominate a different auth.json for writes"
    );
    assert_eq!(
        std::fs::read_to_string(unrelated).unwrap(),
        "unrelated document"
    );
    assert_no_token_in(&out);
}

/// Every non-empty stdout line, parsed as JSON.
fn events(out: &Output) -> Vec<serde_json::Value> {
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|err| panic!("stdout line is not one JSON object ({err}): {line}"))
        })
        .collect()
}

fn output_details(out: &Output) -> String {
    format!(
        "exit {:?}\nstdout: {}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// `--from-codex --non-interactive` speaks the login protocol: the import's cost as one
/// `notice`, then each notice that holds (here the first-account one: codex has a login of its
/// own and no older `tcr` is in reach), then `saved` with the provider and the plan, and no
/// prose on stdout. A refused import prints one coded `error` line, and the same reason on one
/// stderr line.
///
/// Watched red: with `saved` emitted before the notices, the third line is a notice.
#[test]
fn from_codex_non_interactive_emits_saved_notice_and_error() {
    let (home, config) = scratch_home("[]");
    let codex_home = home.path().join(".codex");
    std::fs::create_dir_all(&codex_home).expect("creating the scratch ~/.codex");
    std::fs::write(codex_home.join("auth.json"), "").expect("codex's own login, present");
    let auth = home.path().join("auth.json");
    std::fs::write(&auth, fake_codex_auth_json()).expect("writing the fake auth.json");

    let out = login_from_codex(home.path(), &config, Some(&auth), &["--non-interactive"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let consequence = format!(
        "{} {}",
        tcr::oauth::CODEX_IMPORT_CONSEQUENCE,
        tcr::oauth::CODEX_IMPORT_OLDER_PROXY
    );
    assert_eq!(
        events(&out),
        [
            serde_json::json!({"event": "notice", "text": consequence}),
            serde_json::json!({
                "event": "notice",
                "text": tcr::oauth::CODEX_FIRST_ACCOUNT_NOTICE
            }),
            serde_json::json!({
                "account": "alice@example.com",
                "event": "saved",
                "plan": "ChatGPT Pro",
                "provider": "codex"
            }),
        ]
    );
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("Logged in as"),
        "no prose on stdout"
    );
    assert_no_token_in(&out);

    const ROUTED: &str = r#"{"accounts":{"11111111-1111-4111-8111-111111111111":{"workspace_backend_origin":"https://gov.chatgpt.com","account_routing_override":"NO_CONSTRAINT"}}}"#;
    let (home, config, _answered) = scratch_home_answering("[]", ROUTED);
    let auth = home.path().join("codex-auth.json");
    std::fs::write(&auth, fake_codex_auth_json()).expect("writing the fake auth.json");
    let out = login_from_codex(home.path(), &config, Some(&auth), &["--non-interactive"]);
    assert_eq!(out.status.code(), Some(1), "{}", output_details(&out));
    let lines = events(&out);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0]["event"], "error");
    assert_eq!(lines[0]["code"], "codex-routed", "{}", output_details(&out));
    let reason = lines[0]["reason"].as_str().expect("a reason").to_string();
    assert!(
        reason.starts_with(
            "this Codex login's ChatGPT workspace is not served at https://chatgpt.com"
        ) && reason.contains("https://gov.chatgpt.com")
            && reason.ends_with("Nothing was written."),
        "{reason}"
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim_end(),
        reason,
        "the same reason, as one stderr line"
    );
}

/// `--from-claude-code --non-interactive` speaks the login protocol too: here, one uncoded
/// `error` line, its reason the whole chain, and nothing written.
///
/// The override is what makes "no Claude Code login" true on a developer's Mac. Without it,
/// the read asks the login Keychain for the developer's real Claude Code login, and the
/// success path would send that token to api.anthropic.com and write the real account into
/// this test's config and output. CI cannot catch a missing override, because a runner has no
/// such Keychain item. So this never runs without the override, and its failure is watched by
/// breaking production instead: with the Claude import's machine error sent back to prose,
/// stdout holds no JSON line.
///
/// The success path stays untested by design: the profile URL is a constant nothing overrides,
/// and no test may reach it.
#[test]
fn from_claude_code_non_interactive_emits_its_events() {
    let (home, config) = scratch_home("[]");
    let before = std::fs::read(&config).expect("the scratch config is there");
    assert!(
        !home.path().join("no-claude-code-login.json").exists(),
        "the override names a file that does not exist"
    );
    let out = pinned_tcr(home.path())
        .args([
            "login",
            "--from-claude-code",
            "--non-interactive",
            "--config",
        ])
        .arg(&config)
        .output()
        .expect("spawning the built tcr binary");
    assert_eq!(out.status.code(), Some(1));
    let lines = events(&out);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0]["event"], "error");
    assert!(lines[0].get("code").is_none(), "{}", lines[0]);
    let reason = lines[0]["reason"].as_str().unwrap_or_default();
    assert!(
        reason.starts_with(
            "importing the Claude Code login failed: no Claude Code login found on this machine"
        ),
        "{}",
        lines[0]
    );
    assert_eq!(
        std::fs::read(&config).expect("the scratch config is still there"),
        before,
        "nothing was written"
    );
}

/// The terminal form of an import ends by saying how to use the account: the line after the
/// import line names `tcr run --codex`.
#[test]
fn the_text_import_adds_the_run_codex_line() {
    let (home, config) = scratch_home("[]");
    let auth = home.path().join("codex-auth.json");
    std::fs::write(&auth, fake_codex_auth_json()).expect("writing the fake auth.json");
    let out = login_from_codex(home.path(), &config, Some(&auth), &[]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    let lines: Vec<&str> = stderr.lines().collect();
    let imported = lines
        .iter()
        .position(|line| line.starts_with("[tcr] imported as 'alice@example.com'"))
        .unwrap_or_else(|| panic!("no import line: {stderr}"));
    assert_eq!(
        lines.get(imported + 1).copied(),
        Some("[tcr] start a pooled session with `tcr run --codex`"),
        "{stderr}"
    );
}
