//! `tcr import`, end to end against the BUILT BINARY: it finds the logins claude-swap and
//! `CLIProxyAPI` hold, says which are new, writes nothing until `--yes`, then imports only those,
//! and never prints a token.
//!
//! # Isolation
//!
//! Pinned on the spawned [`Command`], never on this test process:
//!
//! * `HOME` → a fresh [`tempfile::TempDir`] ([`crate::tcr_env::tcr`]), so the real config and
//!   the real `~/.claude-swap-backup`, `~/.cli-proxy-api`, `~/.claude.json` and `~/.codex` are
//!   never read. Each store's override (`TCR_CLAUDE_SWAP_DIR`, `TCR_CLIPROXYAPI_DIRS`,
//!   `TCR_CLAUDE_CODE_ACCOUNT`, `TCR_CODEX_AUTH`) names a path inside that tempdir too.
//! * `TCR_NO_KEYCHAIN=1` and a `TCR_CLAUDE_CODE_CREDENTIALS` that does not exist
//!   ([`crate::tcr_env::isolate`]): a scratch `HOME` does not scratch the login Keychain, which
//!   holds the developer's real Claude Code login and any real claude-swap slots.
//! * `proxy.port` → `0`, which names no listener, so `--yes` never reaches a real proxy on 3456;
//!   and `codexUpstream` → a loopback stub that clears the Codex fixture's workspace.
//!
//! Every token below is fake. The Codex JWTs are put together at run time, as
//! `tests/codex_import.rs` does, because the dotted form written out whole is what a secret
//! scanner's JWT rule matches. The Claude fixtures' profile fetch, under `--yes`, goes to the real
//! profile endpoint with a fake bearer and is refused, so the rows are named from the identity
//! each source stores, which is the fallback under test.
use std::path::{Path, PathBuf};
use std::process::Output;

/// `{"alg":"none","typ":"JWT"}`.
const JWT_HEADER: &str = "eyJhbGciOiJub25lIiwidHlwIjoiSldUIn0";
/// `{"exp":1800000000,"https://api.openai.com/auth":{"chatgpt_account_id":
/// "11111111-1111-4111-8111-111111111111","chatgpt_plan_type":"pro"}}`.
const ACCESS_PAYLOAD: &str = "eyJleHAiOjE4MDAwMDAwMDAsImh0dHBzOi8vYXBpLm9wZW5haS5jb20vYXV0aCI6eyJjaGF0Z3B0X2FjY291bnRfaWQiOiIxMTExMTExMS0xMTExLTQxMTEtODExMS0xMTExMTExMTExMTEiLCJjaGF0Z3B0X3BsYW5fdHlwZSI6InBybyJ9fQ";
/// The same account, plus `"email":"alice@example.com"` and a `chatgpt_user_id`.
const ID_PAYLOAD: &str = "eyJleHAiOjE4MDAwMDAwMDAsImVtYWlsIjoiYWxpY2VAZXhhbXBsZS5jb20iLCJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiMTExMTExMTEtMTExMS00MTExLTgxMTEtMTExMTExMTExMTExIiwiY2hhdGdwdF9wbGFuX3R5cGUiOiJwcm8iLCJjaGF0Z3B0X3VzZXJfaWQiOiJ1c2VyLWZha2UifX0";

/// Clears the fixture Codex login's workspace: served at chatgpt.com, no routing override.
const UNCONSTRAINED_WORKSPACE: &str = r#"{"accounts":{"11111111-1111-4111-8111-111111111111":{"workspace_backend_origin":"NO_CONSTRAINT","account_routing_override":"NO_CONSTRAINT","plan_type":"pro"}}}"#;
/// Routes the same workspace elsewhere, so its import is refused.
const ROUTED_WORKSPACE: &str = r#"{"accounts":{"11111111-1111-4111-8111-111111111111":{"workspace_backend_origin":"https://gov.example.com","account_routing_override":"us","plan_type":"pro"}}}"#;

/// An expiry well in the future, so `--yes` never refreshes a fixture against a real endpoint.
const FAR_FUTURE_MS: i64 = 4_102_444_800_000;

const ALICE_UUID: &str = "11111111-1111-4111-8111-aaaaaaaaaaaa";
const BOB_UUID: &str = "22222222-2222-4222-8222-bbbbbbbbbbbb";
const CAROL_UUID: &str = "33333333-3333-4333-8333-cccccccccccc";
const HENRY_UUID: &str = "66666666-6666-4666-8666-dddddddddddd";
const ORG_ONE: &str = "44444444-4444-4444-8444-444444444444";
const ORG_TWO: &str = "55555555-5555-4555-8555-555555555555";

/// The token text tcr already holds for bob, which an import must never replace.
const BOB_TCR_ACCESS: &str = "at-fake-tcr-own-bob";
const BOB_TCR_REFRESH: &str = "rt-fake-tcr-own-bob";

/// Every token a fixture holds or tcr already holds: none may reach stdout or stderr.
fn every_fixture_token() -> Vec<String> {
    vec![
        "at-fake-swap-alice".to_string(),
        "rt-fake-swap-alice".to_string(),
        "at-fake-swap-bob".to_string(),
        "rt-fake-swap-bob".to_string(),
        "sk-ant-api03-fake-swap-key".to_string(),
        "at-fake-cpa-carol".to_string(),
        "rt-fake-cpa-carol".to_string(),
        "at-fake-cpa-bob".to_string(),
        "rt-fake-cpa-bob".to_string(),
        "rt-fake-cpa-codex".to_string(),
        "rt-fake-wrong-shape".to_string(),
        "sk-fake-cpa-api-key".to_string(),
        "rt-fake-enc-wrong-type".to_string(),
        BOB_TCR_ACCESS.to_string(),
        BOB_TCR_REFRESH.to_string(),
        "at-fake-tcr-own-henry".to_string(),
        "rt-fake-tcr-own-henry".to_string(),
        ACCESS_PAYLOAD.to_string(),
        ID_PAYLOAD.to_string(),
    ]
}

/// A Claude Code credentials document, the shape claude-swap backs up.
fn claude_code_document(access: &str, refresh: &str) -> String {
    format!(
        r#"{{"claudeAiOauth":{{"accessToken":"{access}","refreshToken":"{refresh}","expiresAt":{FAR_FUTURE_MS},"scopes":["user:inference","user:profile"],"subscriptionType":"max"}}}}"#
    )
}

/// Standard base64, padded, as Python's `base64.b64encode` writes claude-swap's `.enc` files.
fn base64_standard(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// A scratch machine: a tcr config already holding bob in org one, a claude-swap store, and a
/// `CLIProxyAPI` auth folder.
struct Machine {
    home: tempfile::TempDir,
    config: PathBuf,
    swap: PathBuf,
    cpa: PathBuf,
}

impl Machine {
    fn new(workspace_answer: &'static str) -> Self {
        let home = tempfile::tempdir().expect("a scratch HOME must be creatable");
        let config = home.path().join(".config").join(tcr::paths::CONFIG_FILE);
        std::fs::create_dir_all(config.parent().expect("the config has a parent"))
            .expect("creating the scratch .config dir");
        let (upstream, _answered) =
            tcr::test_support::stub_upstream::json_answering(workspace_answer, || {});
        let upstream = upstream.url();
        std::fs::write(
            &config,
            format!(
                r#"{{ "proxy": {{ "port": 0 }}, "codexUpstream": "{upstream}", "accounts": [
                  {{ "name": "bob@example.com", "accountType": "oauth", "accountUuid": "{BOB_UUID}",
                     "orgUuid": "{ORG_ONE}", "accessToken": "{BOB_TCR_ACCESS}",
                     "refreshToken": "{BOB_TCR_REFRESH}", "expiresAt": {FAR_FUTURE_MS} }},
                  {{ "name": "henry@example.com", "accountType": "oauth", "accountUuid": "{HENRY_UUID}",
                     "orgUuid": "{ORG_TWO}", "accessToken": "at-fake-tcr-own-henry",
                     "refreshToken": "rt-fake-tcr-own-henry", "expiresAt": {FAR_FUTURE_MS} }}
                ] }}"#
            ),
        )
        .expect("writing the scratch config");
        let swap = home.path().join("swap-store");
        let cpa = home.path().join("cpa-auth");
        let machine = Self {
            home,
            config,
            swap,
            cpa,
        };
        machine.write_claude_swap();
        machine.write_cli_proxy_api();
        machine
    }

    /// Slot 1 alice (new), slot 2 bob in org one (already in tcr), slot 3 an API key, slot 4 the
    /// active account, slot 5 an `.enc` whose document has a token where a number belongs, slot 6
    /// henry, already in tcr, with no stored login at all: the identity check must come first.
    fn write_claude_swap(&self) {
        let credentials = self.swap.join("credentials");
        std::fs::create_dir_all(&credentials).expect("creating the claude-swap store");
        std::fs::write(
            self.swap.join("sequence.json"),
            format!(
                r#"{{"activeAccountNumber":4,"lastUpdated":"2026-10-17T00:00:00Z","sequence":[1,2,3,4,5,6],"accounts":{{
                  "1":{{"email":"alice@example.com","uuid":"{ALICE_UUID}","organizationUuid":"{ORG_ONE}","organizationName":"Alice Org"}},
                  "2":{{"email":"bob@example.com","uuid":"{BOB_UUID}","organizationUuid":"{ORG_ONE}","organizationName":"Bob Org"}},
                  "3":{{"email":"api-key-3@token.local","uuid":"","organizationUuid":null,"kind":"api_key"}},
                  "4":{{"email":"erin@example.com","uuid":"{CAROL_UUID}","organizationUuid":"{ORG_TWO}"}},
                  "5":{{"email":"frank@example.com","uuid":"{CAROL_UUID}","organizationUuid":"{ORG_ONE}"}},
                  "6":{{"email":"henry@example.com","uuid":"{HENRY_UUID}","organizationUuid":"{ORG_TWO}"}}
                }}}}"#
            ),
        )
        .expect("writing sequence.json");
        let enc = |n: u8, email: &str, document: &str| {
            std::fs::write(
                credentials.join(format!(".creds-{n}-{email}.enc")),
                base64_standard(document.as_bytes()),
            )
            .expect("writing a .enc file");
        };
        enc(
            1,
            "alice@example.com",
            &claude_code_document("at-fake-swap-alice", "rt-fake-swap-alice"),
        );
        enc(
            2,
            "bob@example.com",
            &claude_code_document("at-fake-swap-bob", "rt-fake-swap-bob"),
        );
        enc(3, "api-key-3@token.local", "sk-ant-api03-fake-swap-key");
        enc(
            5,
            "frank@example.com",
            r#"{"claudeAiOauth":{"accessToken":"x","expiresAt":"rt-fake-enc-wrong-type"}}"#,
        );
    }

    /// A Claude file for carol (new), one for bob in org one (already in tcr, and read, unlike
    /// his claude-swap slot), a Codex file (new), an API-key file, a file with a token in a field
    /// of the wrong type, a file that is not JSON, and a gemini file tcr ignores.
    fn write_cli_proxy_api(&self) {
        std::fs::create_dir_all(&self.cpa).expect("creating the CLIProxyAPI folder");
        let file = |name: &str, body: String| {
            std::fs::write(self.cpa.join(name), body).expect("writing an auth file");
        };
        file(
            "claude-bob@example.com.json",
            format!(
                r#"{{"type":"claude","access_token":"at-fake-cpa-bob","refresh_token":"rt-fake-cpa-bob","email":"bob@example.com","account_uuid":"{BOB_UUID}","organization_uuid":"{ORG_ONE}","expired":"2100-01-01T00:00:00Z"}}"#
            ),
        );
        file(
            "claude-carol@example.com.json",
            format!(
                r#"{{"type":"claude","access_token":"at-fake-cpa-carol","refresh_token":"rt-fake-cpa-carol","email":"carol@example.com","account_uuid":"{CAROL_UUID}","organization_uuid":"{ORG_ONE}","organization_name":"Carol Org","expired":"2100-01-01T00:00:00Z","last_refresh":"2026-10-17T00:00:00Z"}}"#
            ),
        );
        file(
            "codex-alice@example.com-pro.json",
            format!(
                r#"{{"type":"codex","id_token":"{JWT_HEADER}.{ID_PAYLOAD}.sig","access_token":"{JWT_HEADER}.{ACCESS_PAYLOAD}.sig","refresh_token":"rt-fake-cpa-codex","account_id":"11111111-1111-4111-8111-111111111111","email":"alice@example.com","plan_type":"pro","expired":"2027-01-15T08:00:00Z"}}"#
            ),
        );
        file(
            "claude-key.json",
            r#"{"type":"claude","api_key":"sk-fake-cpa-api-key","email":"gail@example.com"}"#
                .to_string(),
        );
        file(
            "codex-wrong-shape.json",
            r#"{"type":"codex","refresh_token":"x","use_siwc":"rt-fake-wrong-shape"}"#.to_string(),
        );
        file("broken.json", "{ not json".to_string());
        file(
            "gemini-someone.json",
            r#"{"type":"gemini","access_token":"ya29-fake"}"#.to_string(),
        );
    }

    fn import(&self, args: &[&str]) -> Output {
        crate::tcr_env::tcr(self.home.path())
            .arg("import")
            .arg("--config")
            .arg(&self.config)
            .args(args)
            .env("TCR_CLAUDE_SWAP_DIR", &self.swap)
            .env("TCR_CLIPROXYAPI_DIRS", &self.cpa)
            .env(
                "TCR_CLAUDE_CODE_ACCOUNT",
                self.home.path().join("no-claude.json"),
            )
            .env(
                "TCR_CODEX_AUTH",
                self.home.path().join("no-codex-auth.json"),
            )
            .output()
            .unwrap_or_else(|err| panic!("spawning the built tcr binary failed: {err}"))
    }

    fn accounts(&self) -> Vec<serde_json::Value> {
        let text = std::fs::read_to_string(&self.config).expect("the config is there");
        let document: serde_json::Value = serde_json::from_str(&text).expect("a readable config");
        document["accounts"].as_array().cloned().unwrap_or_default()
    }
}

fn text(output: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Fails naming the token when any fixture token appears in `stdout` or `stderr`.
fn assert_no_token_text(stdout: &str, stderr: &str) {
    for token in every_fixture_token() {
        assert!(
            !stdout.contains(&token) && !stderr.contains(&token),
            "token text {token} reached the output.\nstdout={stdout}\nstderr={stderr}"
        );
    }
}

/// Every path under `dir`, with its bytes, sorted: the whole store, to compare before and after.
fn snapshot(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        for entry in std::fs::read_dir(&next).expect("reading a fixture dir") {
            let path = entry.expect("a dir entry").path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let bytes = std::fs::read(&path).expect("reading a fixture file");
                out.push((path, bytes));
            }
        }
    }
    out.sort();
    out
}

/// The listing line naming `needle`, its column padding collapsed to single spaces.
fn line_for(stdout: &str, needle: &str) -> String {
    let Some(line) = stdout.lines().find(|line| line.contains(needle)) else {
        panic!("no line for {needle}:\n{stdout}");
    };
    line.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The listing names every entry with its verdict and writes nothing: the config, the
/// claude-swap store and the `CLIProxyAPI` folder are byte-for-byte what they were.
///
/// Watched red against a listing that saves the config it read.
#[test]
fn the_listing_marks_each_login_and_writes_nothing() {
    let machine = Machine::new(UNCONSTRAINED_WORKSPACE);
    let config_before = std::fs::read(&machine.config).expect("the config");
    let swap_before = snapshot(&machine.swap);
    let cpa_before = snapshot(&machine.cpa);

    let output = machine.import(&[]);
    let (stdout, stderr) = text(&output);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout={stdout}\nstderr={stderr}"
    );

    assert_eq!(
        std::fs::read(&machine.config).expect("the config"),
        config_before,
        "`tcr import` must leave the config byte-for-byte alone"
    );
    assert_eq!(
        snapshot(&machine.swap),
        swap_before,
        "the claude-swap store is untouched"
    );
    assert_eq!(
        snapshot(&machine.cpa),
        cpa_before,
        "the CLIProxyAPI folder is untouched"
    );

    // First, so a listing that reads a slot before checking whether tcr already has it fails
    // here: henry's slot holds no login at all, so reading it first says "skipped".
    assert!(
        line_for(&stdout, "claude-swap slot 6").ends_with("already in tcr as 'henry@example.com'"),
        "a slot tcr already has is never read: {stdout}"
    );
    assert!(
        line_for(&stdout, "claude-swap slot 1").ends_with("claude alice@example.com new"),
        "{stdout}"
    );
    assert!(
        line_for(&stdout, "claude-swap slot 2").ends_with("already in tcr as 'bob@example.com'"),
        "{stdout}"
    );
    assert!(
        line_for(&stdout, "claude-swap slot 3")
            .ends_with("skipped: API key, tcr serves subscriptions only"),
        "{stdout}"
    );
    assert!(
        line_for(&stdout, "claude-swap slot 4")
            .contains("skipped: the account active in Claude Code"),
        "{stdout}"
    );
    assert!(
        line_for(&stdout, "claude-swap slot 5").contains("skipped:"),
        "{stdout}"
    );
    assert!(
        line_for(&stdout, "claude-bob@example.com.json")
            .ends_with("already in tcr as 'bob@example.com'"),
        "{stdout}"
    );
    assert!(
        line_for(&stdout, "claude-carol@example.com.json")
            .ends_with("claude carol@example.com new"),
        "{stdout}"
    );
    assert!(
        line_for(&stdout, "codex-alice@example.com-pro.json")
            .ends_with("codex alice@example.com new"),
        "{stdout}"
    );
    assert!(
        line_for(&stdout, "claude-key.json")
            .ends_with("skipped: API key, tcr serves subscriptions only"),
        "{stdout}"
    );
    assert!(
        line_for(&stdout, "codex-wrong-shape.json").contains("skipped:"),
        "{stdout}"
    );
    assert!(
        line_for(&stdout, "broken.json").contains("skipped:"),
        "{stdout}"
    );
    assert!(
        !stdout.contains("gemini"),
        "another tool's type is not listed: {stdout}"
    );
    assert!(stdout.contains("tcr import --yes"), "{stdout}");
    assert_no_token_text(&stdout, &stderr);
}

/// `--yes` imports the three new logins and leaves bob, already in tcr, exactly as tcr holds
/// him: tcr's own token, not claude-swap's copy. The take is said once, before the writes.
///
/// Watched red against a `--yes` that also imports the logins already in tcr.
#[test]
fn yes_imports_the_new_logins_and_leaves_the_present_ones_alone() {
    let machine = Machine::new(UNCONSTRAINED_WORKSPACE);
    let output = machine.import(&["--yes"]);
    let (stdout, stderr) = text(&output);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout={stdout}\nstderr={stderr}"
    );

    let accounts = machine.accounts();
    let row = |name: &str| {
        accounts
            .iter()
            .find(|row| row["name"] == name)
            .unwrap_or_else(|| panic!("no row {name}: {accounts:#?}"))
            .clone()
    };
    let bob = row("bob@example.com");
    assert_eq!(
        bob["accessToken"], BOB_TCR_ACCESS,
        "bob keeps tcr's own token"
    );
    assert_eq!(
        bob["refreshToken"], BOB_TCR_REFRESH,
        "bob keeps tcr's own token"
    );

    let alice = row("alice@example.com");
    assert_eq!(alice["refreshToken"], "rt-fake-swap-alice");
    assert_eq!(
        alice["accountUuid"], ALICE_UUID,
        "named from claude-swap's stored identity"
    );
    assert_eq!(alice["orgUuid"], ORG_ONE);
    let carol = row("carol@example.com");
    assert_eq!(carol["refreshToken"], "rt-fake-cpa-carol");
    assert_eq!(carol["expiresAt"], FAR_FUTURE_MS);
    let codex = accounts
        .iter()
        .find(|row| row["provider"] == "codex")
        .unwrap_or_else(|| panic!("no Codex row: {accounts:#?}"));
    assert_eq!(codex["refreshToken"], "rt-fake-cpa-codex");
    assert_eq!(
        codex.get("codexHome"),
        None,
        "a CLIProxyAPI login records no folder to write refreshed tokens back to"
    );
    assert_eq!(
        accounts.len(),
        5,
        "bob, henry and three imports: {accounts:#?}"
    );

    assert_eq!(
        stderr.matches("single-use").count(),
        1,
        "the take is said once: {stderr}"
    );
    assert!(
        stderr.contains("claude-swap and CLIProxyAPI"),
        "the take names both tools: {stderr}"
    );
    assert!(
        stdout.contains("imported claude-swap alice@example.com as 'alice@example.com'"),
        "{stdout}"
    );
    assert_no_token_text(&stdout, &stderr);

    // A second run finds every importable login already in tcr and writes nothing.
    let after_first = std::fs::read(&machine.config).expect("the config");
    let again = machine.import(&["--yes"]);
    let (stdout, stderr) = text(&again);
    assert_eq!(
        again.status.code(),
        Some(0),
        "stdout={stdout}\nstderr={stderr}"
    );
    assert!(stdout.contains("nothing to import"), "{stdout}");
    assert_eq!(
        std::fs::read(&machine.config).expect("the config"),
        after_first
    );
}

/// An import that fails is one line naming why, the others still land, the run exits non-zero,
/// and no token reaches the output on that path either. The Codex login's workspace is routed
/// elsewhere, so its import is refused.
///
/// Watched red against a skip reason that quotes serde's message, which echoes the wrongly typed
/// value (a token) in `codex-wrong-shape.json`.
#[test]
fn a_failed_import_is_one_line_and_no_path_prints_a_token() {
    let machine = Machine::new(ROUTED_WORKSPACE);
    let output = machine.import(&["--yes"]);
    let (stdout, stderr) = text(&output);
    // First: it is what this test is for, and a failure count that moved must not hide a leak.
    assert_no_token_text(&stdout, &stderr);
    assert_ne!(
        output.status.code(),
        Some(0),
        "stdout={stdout}\nstderr={stderr}"
    );
    assert!(
        stderr.contains("[tcr] could not import CLIProxyAPI alice@example.com"),
        "{stderr}"
    );
    assert!(
        stderr.contains("1 of 3 new logins could not be imported"),
        "{stderr}"
    );
    let accounts = machine.accounts();
    assert!(
        accounts.iter().all(|row| row["provider"] != "codex"),
        "the refused Codex login is not written: {accounts:#?}"
    );
    assert_eq!(
        accounts.len(),
        4,
        "bob, henry, alice and carol: {accounts:#?}"
    );
}
