//! `tcr login --codex`, end to end against the BUILT BINARY, with a fake `codex`.
//!
//! # Isolation
//!
//! Pinned on the spawned [`Command`], never on this test process (whose tests run on parallel
//! threads that share one environment):
//!
//! * `HOME` → a fresh [`tempfile::TempDir`], holding the config at
//!   `$HOME/.config/teamclaude.json`, so the real config (working credentials for real accounts)
//!   and the real `~/.codex` are never read or written. Every sign-in folder tcr makes lands
//!   beside that config, inside the tempdir.
//! * `proxy.port` in that config → `0`, which cannot name an actual listener, so the route probe
//!   cannot hit another test's reused ephemeral port or a real proxy on 3456.
//! * `codexUpstream` in that config → a stub on loopback ([`spawn_upstream`]) that clears the
//!   fake workspaces, so the import's workspace check never reaches chatgpt.com.
//! * `TCR_CODEX_BIN` → a fake `codex`, a shell script in the tempdir that logs its
//!   `CODEX_HOME`, prints the lines a test prepared, copies a prepared `auth.json` (its JWTs put
//!   together at run time) into `CODEX_HOME`, and exits with the code the test chose. No test
//!   runs a real `codex`, and its script uses absolute paths, because `PATH` is empty.
//! * `PATH` → an empty directory, `TCR_CODEX_SEARCH_DIRS` → empty directories, and
//!   `TCR_BUNDLED_TCR` → a path that does not exist: `/opt/homebrew/bin`, `/usr/local/bin` and
//!   `/Applications` are not under a scratch `HOME`, and without these the search and the
//!   older-tcr check would find the developer's own `codex` and `tcr`.
//! * `CODEX_HOME`, `TCR_CODEX_AUTH` and `XDG_CACHE_HOME` removed, and
//!   `TCR_CLAUDE_CODE_CREDENTIALS` → a file that does not exist, so nothing reads the login
//!   Keychain.
//!
//! The events are compared, as parsed values, with the lines of
//! `apps/macos/Tests/Fixtures/codex-login-events.jsonl`, the fixture TcrBar's decoder is tested on.
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// `{"alg":"none","typ":"JWT"}`.
const JWT_HEADER: &str = "eyJhbGciOiJub25lIiwidHlwIjoiSldUIn0";
/// bob's ChatGPT user and workspace.
const BOB_USER: &str = "55555555-5555-4555-8555-555555555555";
const BOB_WORKSPACE: &str = "66666666-6666-4666-8666-666666666666";
/// The authorize URL every fake prints.
const AUTHORIZE_URL: &str =
    "https://auth.openai.com/oauth/authorize?client_id=app_EXAMPLE&state=EXAMPLE";
/// An accounts/check answer that clears bob's workspace for the pool.
const BOB_CLEARED: &str = r#"{"accounts":{"66666666-6666-4666-8666-666666666666":{"workspace_backend_origin":"NO_CONSTRAINT","account_routing_override":"NO_CONSTRAINT","plan_type":"pro"}}}"#;

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

/// bob's login: `bob@example.com`, [`BOB_USER`] in [`BOB_WORKSPACE`], on Pro.
fn bob_auth() -> String {
    auth_json_for("bob@example.com", BOB_USER, BOB_WORKSPACE, "pro")
}

/// The lines of codex's own browser sign-in, as it prints them on stderr: the one `eprintln!` of
/// the browser flow in openai/codex's login crate at f82e365 (`codex-rs/login/`).
fn browser_lines() -> String {
    format!(
        "Starting local login server on http://localhost:1455.\nIf your browser did not open, \
         navigate to this URL to authenticate:\n\n{AUTHORIZE_URL}\n\nOn a remote or headless \
         machine? Use `codex login --device-auth` instead.\n"
    )
}

/// The fixture's lines, parsed.
fn fixture() -> Vec<serde_json::Value> {
    let text = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("apps/macos/Tests/Fixtures/codex-login-events.jsonl"),
    )
    .expect("the committed fixture");
    text.lines()
        .map(|line| serde_json::from_str(line).expect("each fixture line is one JSON object"))
        .collect()
}

/// A stub `codexUpstream` on loopback that answers every request `200` with `answer`, calling
/// `seen` first. Returns its base URL and how many requests it answered.
fn spawn_upstream(
    answer: &'static str,
    seen: impl Fn() + Send + 'static,
) -> (String, Arc<AtomicUsize>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("binding the stub upstream");
    let addr = listener.local_addr().expect("an address");
    let answered = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&answered);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(&mut stream);
            let mut line = String::new();
            while reader.read_line(&mut line).is_ok_and(|n| n > 0) && line != "\r\n" {
                line.clear();
            }
            seen();
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

/// One scratch machine: a `HOME`, its config, a fake `codex`, and the files that drive it.
struct Scratch {
    home: tempfile::TempDir,
    config: PathBuf,
    /// The fake `codex`.
    codex: PathBuf,
    /// What the fake logged: one `home <CODEX_HOME>` line per run, and the folder's mode.
    log: PathBuf,
    /// The workspace check's stub: how many checks it answered.
    answered: Arc<AtomicUsize>,
}

impl Scratch {
    /// A scratch machine whose config holds `accounts` (a JSON array) and whose workspace
    /// check clears bob's workspace.
    fn new(accounts: &str) -> Self {
        Self::with_upstream(accounts, |_| {})
    }

    /// [`Scratch::new`], whose workspace stub calls `seen` with the fake's log on every check.
    fn with_upstream(accounts: &str, seen: impl Fn(&Path) + Send + 'static) -> Self {
        let home = tempfile::tempdir().expect("a scratch HOME");
        let config_dir = home.path().join(".config");
        std::fs::create_dir_all(&config_dir).expect("the scratch .config");
        let config = config_dir.join(tcr::paths::CONFIG_FILE);
        let log = home.path().join("codex.log");
        let watched = log.clone();
        let (upstream, answered) = spawn_upstream(BOB_CLEARED, move || seen(&watched));
        std::fs::write(
            &config,
            format!(
                r#"{{ "proxy": {{ "port": 0 }}, "codexUpstream": "{upstream}", "accounts": {accounts} }}"#
            ),
        )
        .expect("writing the scratch config");
        for dir in ["empty-path", "empty-search"] {
            std::fs::create_dir_all(home.path().join(dir)).expect("an empty directory");
        }
        let scratch = Self {
            codex: home.path().join("fake-codex"),
            log,
            config,
            answered,
            home,
        };
        scratch.fake_codex(0, "", None);
        scratch
    }

    fn path(&self, name: &str) -> PathBuf {
        self.home.path().join(name)
    }

    /// Make the fake `codex` print `stderr` on stderr, write `auth` (when given) as the login,
    /// and exit with `code`.
    fn fake_codex(&self, code: i32, stderr: &str, auth: Option<&str>) {
        let said = self.path("codex-says.txt");
        std::fs::write(&said, stderr).expect("the fake's lines");
        let login = self.path("codex-login.json");
        match auth {
            Some(auth) => std::fs::write(&login, auth).expect("the fake's login"),
            None => {
                let _ = std::fs::remove_file(&login);
            }
        }
        write_script(
            &self.codex,
            &fake_codex_script(&self.log, &said, &login, code),
        );
    }

    /// The built `tcr`, pinned as the module doc says.
    fn tcr(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_tcr"));
        command
            .env("HOME", self.home.path())
            // Parallel tests spawn many children; bound each child's runtime threads.
            .env("TOKIO_WORKER_THREADS", "2")
            .env("TCR_CODEX_BIN", &self.codex)
            .env_remove("CODEX_BIN")
            .env("PATH", self.path("empty-path"))
            .env("TCR_CODEX_SEARCH_DIRS", self.path("empty-search"))
            .env("TCR_BUNDLED_TCR", self.path("no-bundled-tcr"))
            .env(
                "TCR_CLAUDE_CODE_CREDENTIALS",
                self.path("no-claude-code-login.json"),
            )
            .env_remove("CODEX_HOME")
            .env_remove("TCR_CODEX_AUTH")
            .env_remove("XDG_CACHE_HOME")
            .stdin(Stdio::null());
        command
    }

    /// `tcr login --codex --config <config> <extra…>`, run to the end.
    fn login(&self, extra: &[&str]) -> Output {
        self.login_with(extra, |_| {})
    }

    /// [`Scratch::login`], with one more change to the command first.
    fn login_with(&self, extra: &[&str], change: impl FnOnce(&mut Command)) -> Output {
        let mut command = self.tcr();
        command
            .args(["login", "--codex", "--config"])
            .arg(&self.config)
            .args(extra);
        change(&mut command);
        command.output().expect("spawning the built tcr binary")
    }

    /// Every `CODEX_HOME` the fake was given, in order.
    fn homes(&self) -> Vec<PathBuf> {
        logged(&self.log, "home ")
            .into_iter()
            .map(PathBuf::from)
            .collect()
    }

    fn accounts(&self) -> Vec<serde_json::Value> {
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&self.config).expect("the config"))
                .expect("the config is JSON");
        doc["accounts"].as_array().cloned().unwrap_or_default()
    }
}

/// The values of the log lines that start with `prefix`.
fn logged(log: &Path, prefix: &str) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.strip_prefix(prefix).map(str::to_string))
        .collect()
}

/// The fake `codex`'s script: absolute paths only, POSIX `sh`.
fn fake_codex_script(log: &Path, said: &Path, login: &Path, code: i32) -> String {
    format!(
        "#!/bin/sh\n\
         echo \"home $CODEX_HOME\" >> '{log}'\n\
         echo \"mode $(/bin/ls -ld \"$CODEX_HOME\")\" >> '{log}'\n\
         echo \"argv $*\" >> '{log}'\n\
         /bin/cat '{said}' >&2\n\
         if [ -f '{login}' ]; then /bin/cp '{login}' \"$CODEX_HOME/auth.json\"; fi\n\
         exit {code}\n",
        log = log.display(),
        said = said.display(),
        login = login.display(),
    )
}

fn write_script(path: &Path, body: &str) {
    std::fs::write(path, body).expect("writing a script");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .expect("making it executable");
}

/// Every non-empty stdout line, parsed.
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

fn says(out: &Output) -> String {
    format!(
        "exit {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// The `notice` events' texts, in order.
fn notices(lines: &[serde_json::Value]) -> Vec<String> {
    lines
        .iter()
        .filter(|line| line["event"] == "notice")
        .map(|line| line["text"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// A first Codex account signed in through `--non-interactive`: `browser` (with `opened`,
/// because codex opens the browser itself), `waiting`, the first-account notice and `saved`,
/// exactly the fixture's first four lines. The row is bob's, keyed on his ChatGPT user and
/// workspace, with no `codexHome`: a login of tcr's own shares no folder.
///
/// Watched red: with `CODEX_OPENS_ITS_BROWSER` flipped, line 1 has no `opened`.
#[test]
fn login_codex_streams_browser_waiting_notice_saved() {
    let scratch = Scratch::new("[]");
    std::fs::create_dir_all(scratch.path(".codex")).expect("~/.codex");
    std::fs::write(scratch.path(".codex/auth.json"), "").expect("codex's own login, present");
    scratch.fake_codex(0, &browser_lines(), Some(&bob_auth()));

    let out = scratch.login(&["--non-interactive"]);
    assert_eq!(out.status.code(), Some(0), "{}", says(&out));
    assert_eq!(events(&out), fixture()[..4], "{}", says(&out));
    let accounts = scratch.accounts();
    assert_eq!(accounts.len(), 1, "{accounts:?}");
    assert_eq!(accounts[0]["name"], "bob@example.com");
    assert_eq!(accounts[0]["provider"], "codex");
    assert_eq!(accounts[0]["accountUuid"], BOB_USER);
    assert_eq!(accounts[0]["orgUuid"], BOB_WORKSPACE);
    assert!(accounts[0].get("codexHome").is_none(), "{}", accounts[0]);
}

/// `--account` re-signs only the row's own identity: a login of someone else is refused with
/// the fixture's identity line, before the import asks anything, and nothing is written.
///
/// Watched red: without the identity check, carol's login is written into the config.
#[test]
fn login_codex_with_a_different_identity_writes_nothing() {
    let scratch = Scratch::new(
        r#"[{ "name": "erin@example.com/codex", "provider": "codex", "credentialVersion": 2,
              "accountUuid": "88888888-8888-4888-8888-888888888888",
              "orgUuid": "99999999-9999-4999-8999-999999999999",
              "accessToken": "at-erin", "refreshToken": "rt-erin", "expiresAt": 1789000000000 }]"#,
    );
    scratch.fake_codex(
        0,
        &browser_lines(),
        Some(&auth_json_for(
            "carol@example.com",
            "33333333-3333-4333-8333-333333333333",
            "44444444-4444-4444-8444-444444444444",
            "plus",
        )),
    );
    let before = std::fs::read(&scratch.config).expect("the config");

    let out = scratch.login(&["--account", "erin@example.com/codex", "--non-interactive"]);
    assert_eq!(out.status.code(), Some(1), "{}", says(&out));
    assert_eq!(events(&out).last(), Some(&fixture()[4]), "{}", says(&out));
    assert_eq!(std::fs::read(&scratch.config).expect("the config"), before);
    assert_eq!(
        scratch.answered.load(Ordering::SeqCst),
        0,
        "no workspace check"
    );
}

/// Every sign-in folder lies beside the config, is named `tcr-codex-login.*`, is `0700` while
/// codex runs, and is gone afterwards, whether codex signed in or failed. A failed `codex login`
/// is `codex-login-failed`, naming its exit code and, when it said anything, its last line.
///
/// Watched red: keeping the folder (`into_path`) leaves it behind; dropping the permissions
/// gives it the umask's mode.
#[test]
fn login_codex_removes_its_throwaway_folder() {
    let scratch = Scratch::new("[]");
    scratch.fake_codex(0, &browser_lines(), Some(&bob_auth()));
    let out = scratch.login(&["--non-interactive"]);
    assert_eq!(out.status.code(), Some(0), "{}", says(&out));

    scratch.fake_codex(3, "", None);
    let out = scratch.login(&["--non-interactive"]);
    assert_eq!(out.status.code(), Some(1), "{}", says(&out));
    assert_eq!(
        events(&out),
        [serde_json::json!({
            "code": "codex-login-failed",
            "event": "error",
            "reason": "`codex login` exited with 3 before a login was saved. Nothing was written."
        })]
    );

    scratch.fake_codex(3, "boom: port 1455 is in use\n", None);
    let out = scratch.login(&["--non-interactive"]);
    assert_eq!(out.status.code(), Some(1), "{}", says(&out));
    assert_eq!(
        events(&out)
            .last()
            .and_then(|line| line["reason"].as_str())
            .map(str::to_string),
        Some(
            "`codex login` exited with 3 before a login was saved (codex said: boom: port 1455 \
             is in use). Nothing was written."
                .to_string()
        )
    );

    let homes = scratch.homes();
    assert_eq!(homes.len(), 3, "one folder per run: {homes:?}");
    let config_dir =
        std::fs::canonicalize(scratch.config.parent().expect("a parent")).expect("the config dir");
    for home in &homes {
        let parent = home.parent().expect("a folder has a parent");
        assert_eq!(
            std::fs::canonicalize(parent).expect("the parent exists"),
            config_dir,
            "{home:?} lies beside the config"
        );
        assert!(
            home.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("tcr-codex-login.")),
            "{home:?}"
        );
        assert!(!home.exists(), "{home:?} is gone");
    }
    for mode in logged(&scratch.log, "mode ") {
        assert!(mode.starts_with("drwx------"), "{mode}");
    }
}

/// With no `codex` anywhere, the refusal names every place searched, and nothing is spawned or
/// written.
#[test]
fn login_codex_names_every_directory_it_searched() {
    let scratch = Scratch::new("[]");
    let home = scratch.home.path().display().to_string();
    for dir in ["d1", "d2"] {
        std::fs::create_dir_all(scratch.path(dir)).expect("a search directory");
    }
    let before = std::fs::read(&scratch.config).expect("the config");
    let out = scratch.login_with(&["--non-interactive"], |command| {
        command
            .env_remove("TCR_CODEX_BIN")
            .env_remove("CODEX_BIN")
            .env("TCR_CODEX_SEARCH_DIRS", format!("{home}/d1:{home}/d2"));
    });
    assert_eq!(out.status.code(), Some(1), "{}", says(&out));
    assert_eq!(
        events(&out),
        [serde_json::json!({
            "code": "codex-missing",
            "event": "error",
            "reason": format!(
                "could not find the codex CLI (searched TCR_CODEX_BIN, CODEX_BIN, PATH, \
                 {home}/d1, {home}/d2). Install it, or set TCR_CODEX_BIN to its path. Nothing \
                 was written."
            )
        })]
    );
    assert!(scratch.homes().is_empty(), "nothing was spawned");
    assert_eq!(std::fs::read(&scratch.config).expect("the config"), before);
}

/// A fake `tcr` in `dir` that answers `--version` with `version` and `login --help` with a
/// help that lacks `--from-codex`.
fn fake_tcr(dir: &Path, version: &str) {
    std::fs::create_dir_all(dir).expect("a directory for a fake tcr");
    let binary = dir.join("tcr");
    write_script(
        &binary,
        &format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo \"tcr {version}\"; else echo \
             \"Options: --from-claude-code --from-codex-home <DIR>\"; fi\n"
        ),
    );
    // A fresh script can spend seconds in macOS's loader before its shell starts. Prepare
    // this known fixture before exercising the production probe's five-second deadline.
    let ready = Command::new(binary)
        .arg("--version")
        .env("HOME", dir)
        .env("CODEX_HOME", dir.join("codex-home"))
        .env("PATH", dir)
        .stdin(Stdio::null())
        .output()
        .expect("preparing the fake tcr");
    assert!(ready.status.success(), "{}", says(&ready));
    assert_eq!(ready.stdout, format!("tcr {version}\n").as_bytes());
}

/// Each notice prints only when it holds: the first-account notice for a config with no Codex
/// row, the no-login notice when codex has no login of its own, one warning per older `tcr`
/// in reach (on `PATH`, or in a directory the search covers), never one twice; all before
/// `saved`, and in the terminal each on its own rung, `[tcr] ` or `[tcr] warning: `.
///
/// Watched red: with the no-login notice printed whatever holds, the second account gets one;
/// with the search directories dropped from the older-tcr scan, the hidden one is missed; with
/// the de-duplication removed, the one on both is warned about twice.
#[test]
fn login_codex_prints_its_notices_only_when_they_hold() {
    let n1 = tcr::oauth::CODEX_FIRST_ACCOUNT_NOTICE.to_string();

    // (a) The first account, no codex login, two older tcrs on PATH.
    let scratch = Scratch::new("[]");
    scratch.fake_codex(0, &browser_lines(), Some(&bob_auth()));
    let (t1, t2) = (scratch.path("t1"), scratch.path("t2"));
    fake_tcr(&t1, "0.2.44");
    fake_tcr(&t2, "1.1.18");
    let path = format!("{}:{}", t1.display(), t2.display());
    let n3 = format!(
        "codex needs a login of its own to start, even when pooled, and none is at {}: unless \
         codex keeps its login in the keyring, run `codex login` once",
        scratch.path(".codex/auth.json").display()
    );
    let n2a = format!(
        "the tcr at {} (0.2.44) predates Codex accounts: update it before it next starts — it \
         would read this login as a Claude account and send its token to Anthropic",
        t1.join("tcr").display()
    );
    let n2b = format!(
        "the tcr at {} (1.1.18) predates Codex accounts: update it — until then it refuses this \
         config, so every command it runs fails",
        t2.join("tcr").display()
    );
    let out = scratch.login_with(&["--non-interactive"], |command| {
        command.env("PATH", &path);
    });
    assert_eq!(out.status.code(), Some(0), "{}", says(&out));
    let lines = events(&out);
    assert_eq!(
        notices(&lines),
        [n1.clone(), n3.clone(), n2a.clone(), n2b.clone()]
    );
    assert_eq!(lines.last().expect("a last line")["event"], "saved");

    // (b) A second account, codex has its login, no tcr in reach: no notice at all.
    let scratch = Scratch::new(
        r#"[{ "name": "dana@example.com", "provider": "codex", "credentialVersion": 2,
              "accountUuid": "88888888-8888-4888-8888-888888888888",
              "orgUuid": "99999999-9999-4999-8999-999999999999",
              "accessToken": "at-dana", "refreshToken": "rt-dana", "expiresAt": 1789000000000 }]"#,
    );
    std::fs::create_dir_all(scratch.path(".codex")).expect("~/.codex");
    std::fs::write(scratch.path(".codex/auth.json"), "").expect("codex's own login");
    scratch.fake_codex(0, &browser_lines(), Some(&bob_auth()));
    let out = scratch.login(&["--non-interactive"]);
    assert_eq!(out.status.code(), Some(0), "{}", says(&out));
    assert!(notices(&events(&out)).is_empty(), "{}", says(&out));

    // (c) The terminal form of (a).
    let scratch = Scratch::new("[]");
    scratch.fake_codex(0, &browser_lines(), Some(&bob_auth()));
    fake_tcr(&scratch.path("t1"), "0.2.44");
    let n2a = format!(
        "the tcr at {} (0.2.44) predates Codex accounts: update it before it next starts — it \
         would read this login as a Claude account and send its token to Anthropic",
        scratch.path("t1/tcr").display()
    );
    let n3 = format!(
        "codex needs a login of its own to start, even when pooled, and none is at {}: unless \
         codex keeps its login in the keyring, run `codex login` once",
        scratch.path(".codex/auth.json").display()
    );
    let out = scratch.login_with(&[], |command| {
        command.env("PATH", scratch.path("t1"));
    });
    assert_eq!(out.status.code(), Some(0), "{}", says(&out));
    let stderr = String::from_utf8_lossy(&out.stderr);
    let lines: Vec<&str> = stderr.lines().collect();
    let wanted = [
        "[tcr] signing in a Codex account: `codex login` runs in a folder of tcr's own, so the \
         codex CLI keeps the login it has"
            .to_string(),
        "If your browser did not open, navigate to this URL to authenticate:".to_string(),
        "[tcr] 'bob@example.com' is a Codex account (ChatGPT Pro): Codex sessions started with \
         `tcr run --codex` can use it"
            .to_string(),
        format!("[tcr] {n1}"),
        format!("[tcr] {n3}"),
        format!("[tcr] warning: {n2a}"),
    ];
    let mut at = 0;
    for want in &wanted {
        at += lines[at..]
            .iter()
            .position(|line| line == want)
            .unwrap_or_else(|| panic!("missing, or out of order: {want}\n{stderr}"));
    }
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("Logged in as 'bob@example.com'."),
        "{}",
        says(&out)
    );

    // (d) An older tcr only in a directory the search covers, then on PATH as well.
    let scratch = Scratch::new("[]");
    scratch.fake_codex(0, &browser_lines(), Some(&bob_auth()));
    let hidden = scratch.path("hidden");
    fake_tcr(&hidden, "0.2.44");
    let n2a = format!(
        "the tcr at {} (0.2.44) predates Codex accounts: update it before it next starts — it \
         would read this login as a Claude account and send its token to Anthropic",
        hidden.join("tcr").display()
    );
    let out = scratch.login_with(&["--non-interactive"], |command| {
        command.env("TCR_CODEX_SEARCH_DIRS", &hidden);
    });
    assert_eq!(out.status.code(), Some(0), "{}", says(&out));
    let lines = events(&out);
    let warned: Vec<String> = notices(&lines)
        .into_iter()
        .filter(|text| text.starts_with("the tcr at "))
        .collect();
    assert_eq!(warned, std::slice::from_ref(&n2a), "{}", says(&out));
    assert_eq!(lines.last().expect("a last line")["event"], "saved");

    let scratch = Scratch::new("[]");
    scratch.fake_codex(0, &browser_lines(), Some(&bob_auth()));
    fake_tcr(&hidden, "0.2.44");
    let out = scratch.login_with(&["--non-interactive"], |command| {
        command
            .env("TCR_CODEX_SEARCH_DIRS", &hidden)
            .env("PATH", &hidden);
    });
    assert_eq!(out.status.code(), Some(0), "{}", says(&out));
    let warned: Vec<String> = notices(&events(&out))
        .into_iter()
        .filter(|text| text.starts_with("the tcr at "))
        .collect();
    assert_eq!(warned, [n2a], "warned once: {}", says(&out));
}

/// A `tcr` that answers like 0.2.44 and appends each argv it is run with to `ran`, in `dir`.
fn planted_tcr(dir: &Path, ran: &Path) {
    std::fs::create_dir_all(dir).expect("a directory for a planted tcr");
    write_script(
        &dir.join("tcr"),
        &format!(
            "#!/bin/sh\necho \"$*\" >> '{}'\nif [ \"$1\" = \"--version\" ]; then echo \"tcr \
             0.2.44\"; fi\n",
            ran.display()
        ),
    );
}

/// Only an absolute `PATH` entry is searched for an older `tcr`. A relative entry, `.` above
/// all, names whatever directory the login was started in, so the scan would run a `tcr`
/// planted there, a freshly cloned repository's, even where the shell would never reach it.
/// Here such a `tcr` sits in the working directory with `.` on `PATH`: it is never run and
/// never warned about. The control names the same directory absolutely, and that `tcr` is run
/// and warned about.
///
/// Watched red: without the absolute-entry filter, the planted `tcr` runs and is warned about
/// as `./tcr`.
#[test]
fn a_relative_path_entry_is_never_searched_for_an_older_tcr() {
    let scratch = Scratch::new("[]");
    scratch.fake_codex(0, &browser_lines(), Some(&bob_auth()));
    let cwd = scratch.path("cloned-repo");
    let ran = scratch.path("planted-tcr-ran.txt");
    planted_tcr(&cwd, &ran);
    let out = scratch.login_with(&["--non-interactive"], |command| {
        command
            .env(
                "PATH",
                format!("{}:.", scratch.path("empty-path").display()),
            )
            .current_dir(&cwd);
    });
    assert_eq!(out.status.code(), Some(0), "{}", says(&out));
    assert!(
        !ran.exists(),
        "the tcr in the working directory was run: {}",
        says(&out)
    );
    let warned: Vec<String> = notices(&events(&out))
        .into_iter()
        .filter(|text| text.starts_with("the tcr at "))
        .collect();
    assert!(warned.is_empty(), "{}", says(&out));

    let scratch = Scratch::new("[]");
    scratch.fake_codex(0, &browser_lines(), Some(&bob_auth()));
    let cwd = scratch.path("cloned-repo");
    let ran = scratch.path("planted-tcr-ran.txt");
    planted_tcr(&cwd, &ran);
    let out = scratch.login_with(&["--non-interactive"], |command| {
        command
            .env(
                "PATH",
                format!("{}:{}", scratch.path("empty-path").display(), cwd.display()),
            )
            .current_dir(&cwd);
    });
    assert_eq!(out.status.code(), Some(0), "{}", says(&out));
    assert!(
        ran.exists(),
        "control: on an absolute entry it is asked: {}",
        says(&out)
    );
    let warned: Vec<String> = notices(&events(&out))
        .into_iter()
        .filter(|text| text.starts_with("the tcr at "))
        .collect();
    assert_eq!(warned.len(), 1, "{}", says(&out));
    assert!(
        warned[0].starts_with(&format!(
            "the tcr at {} (0.2.44)",
            cwd.join("tcr").display()
        )),
        "{}",
        says(&out)
    );
}

/// The fake for a cancelled sign-in: it logs its pid, its process group and its `CODEX_HOME`,
/// prints the browser lines, starts a child of its own, logs that child's pid, and waits.
fn hanging_codex_script(log: &Path) -> String {
    format!(
        "#!/bin/sh\n\
         echo \"pid $$\" >> '{log}'\n\
         echo \"pgid $(/bin/ps -o pgid= -p $$ | /usr/bin/tr -d ' ')\" >> '{log}'\n\
         echo \"home $CODEX_HOME\" >> '{log}'\n\
         printf '%s\\n' 'If your browser did not open, navigate to this URL to authenticate:' '' \
         '{AUTHORIZE_URL}' >&2\n\
         /bin/sleep 60 &\n\
         echo \"child $!\" >> '{log}'\n\
         wait\n",
        log = log.display(),
    )
}

/// Whether `pid` is gone, or a zombie nobody will run again.
fn gone(expected: (u32, u64)) -> bool {
    let pid = expected.0;
    use sysinfo::{Pid, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System};
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
        true,
        ProcessRefreshKind::nothing(),
    );
    system.process(Pid::from_u32(pid)).is_none_or(|process| {
        process.status() == ProcessStatus::Zombie
            || !cleanup_identity_is_current(expected, (pid, process.start_time()))
    })
}

fn cleanup_identity_is_current(expected: (u32, u64), current: (u32, u64)) -> bool {
    expected.0 == current.0 && expected.1 > 0 && expected.1 == current.1
}

#[test]
fn cleanup_will_not_signal_a_reused_fixture_pid() {
    assert!(!cleanup_identity_is_current((111, 100), (111, 101)));
    assert!(cleanup_identity_is_current((111, 100), (111, 100)));
    assert!(!cleanup_identity_is_current((111, 0), (111, 0)));
}

fn kill_hard(expected: (u32, u64)) {
    let pid = expected.0;
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, Signal, System};
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
        true,
        ProcessRefreshKind::nothing(),
    );
    if let Some(process) = system.process(Pid::from_u32(pid)) {
        if process.status() != sysinfo::ProcessStatus::Zombie
            && cleanup_identity_is_current(expected, (pid, process.start_time()))
        {
            let _ = process.kill_with(Signal::Kill);
        }
    }
}

/// Every signal is guarded by the exact PID and start time captured while the fixture lives.
struct TestProcessCleanup {
    identities: Vec<(u32, u64)>,
}

impl TestProcessCleanup {
    fn new(pid: u32) -> Self {
        let mut cleanup = Self {
            identities: Vec::new(),
        };
        cleanup.remember(pid);
        cleanup
    }

    fn remember(&mut self, pid: u32) {
        use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
        let mut system = System::new();
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
            true,
            ProcessRefreshKind::nothing(),
        );
        if let Some(process) = system.process(Pid::from_u32(pid)) {
            self.identities.push((pid, process.start_time()));
        }
    }
}

#[test]
fn fixture_cleanup_stops_an_owned_process_even_without_signal_forwarding() {
    let home = tempfile::tempdir().unwrap();
    let mut process = Command::new("/bin/sh")
        .args(["-c", "while :; do /bin/sleep 1; done"])
        .env("HOME", home.path())
        .env("CODEX_HOME", home.path().join(".codex"))
        .spawn()
        .unwrap();
    let identity;
    {
        let cleanup = TestProcessCleanup::new(process.id());
        identity = cleanup.identities[0];
    }
    let cleaned = within(Duration::from_millis(500), || gone(identity));
    // The red control must clean up as well; assertions never leave a looping fixture.
    kill_hard(identity);
    let _ = process.wait();
    assert!(
        cleaned,
        "dropping fixture cleanup must stop its exact owned process"
    );
}

impl Drop for TestProcessCleanup {
    fn drop(&mut self) {
        for &identity in self.identities.iter().rev() {
            kill_hard(identity);
        }
    }
}

/// Poll `done` for up to `limit`.
fn within(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        if done() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// A cancelled sign-in stops `codex login` and everything it started, deletes the folder,
/// writes nothing, and ends tcr with an exit code, not by the signal: (a) for a SIGTERM sent
/// to tcr alone, which tcr passes on to codex and codex's children; (b) for a SIGTERM sent to
/// tcr's whole process group, as TcrBar's Cancel sends it, which codex shares because tcr
/// spawns it in its own group.
///
/// Watched red: without the handler, tcr dies by the signal and leaves the folder, and in (a)
/// the fake keeps running; signalling codex's pid alone leaves its child alive in (a); spawning
/// codex in a group of its own fails (b)'s group check.
#[test]
fn login_codex_stops_codex_and_removes_its_folder_when_cancelled() {
    for group in [false, true] {
        let scratch = Scratch::new("[]");
        write_script(&scratch.codex, &hanging_codex_script(&scratch.log));
        let before = std::fs::read(&scratch.config).expect("the config");
        let mut command = scratch.tcr();
        command
            .args(["login", "--codex", "--non-interactive", "--config"])
            .arg(&scratch.config)
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if group {
            std::os::unix::process::CommandExt::process_group(&mut command, 0);
        }
        let mut child = command.spawn().expect("spawning the built tcr binary");
        let tcr = child.id();
        let stdout = child.stdout.take().expect("piped stdout");
        let (lines, received) = std::sync::mpsc::channel::<String>();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let _ = lines.send(line);
            }
        });
        let browser = received
            .recv_timeout(Duration::from_secs(20))
            .expect("the browser event");
        assert!(browser.contains(r#""event":"browser""#), "{browser}");
        assert!(
            within(Duration::from_secs(10), || !logged(&scratch.log, "child ")
                .is_empty()),
            "the fake started its child"
        );
        let pids: Vec<(u32, u64)> = ["pid ", "child "]
            .iter()
            .flat_map(|prefix| logged(&scratch.log, prefix))
            .map(|pid| {
                use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
                let pid: u32 = pid.trim().parse().expect("a logged pid");
                let mut system = System::new();
                system.refresh_processes_specifics(
                    ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
                    true,
                    ProcessRefreshKind::nothing(),
                );
                let process = system
                    .process(Pid::from_u32(pid))
                    .expect("the owned fixture is running");
                (pid, process.start_time())
            })
            .collect();
        let home = PathBuf::from(&scratch.homes()[0]);

        if group {
            let sent = Command::new("/bin/kill")
                .args(["-s", "TERM", "--", &format!("-{tcr}")])
                .status()
                .expect("/bin/kill");
            assert!(sent.success(), "the group was signalled");
        } else {
            use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, Signal, System};
            let mut system = System::new();
            system.refresh_processes_specifics(
                ProcessesToUpdate::Some(&[Pid::from_u32(tcr)]),
                true,
                ProcessRefreshKind::nothing(),
            );
            let process = system.process(Pid::from_u32(tcr)).expect("tcr is running");
            assert_eq!(process.kill_with(Signal::Term), Some(true));
        }

        let mut status = None;
        let exited = within(Duration::from_secs(5), || {
            status = child.try_wait().expect("polling tcr");
            status.is_some()
        });
        let all_gone = within(Duration::from_secs(5), || pids.iter().all(|&pid| gone(pid)));
        for &pid in &pids {
            kill_hard(pid);
        }
        if !exited {
            let _ = child.kill();
            let _ = child.wait();
        }
        assert!(exited, "group={group}: tcr exits within 5 s");
        let status = status.expect("an exit status");
        assert!(
            status.code().is_some(),
            "group={group}: tcr ends with an exit code, not by the signal: {status:?}"
        );
        if !group {
            assert_eq!(status.code(), Some(143), "128 plus SIGTERM's number");
        }
        assert!(
            all_gone,
            "group={group}: every process codex started is gone: {pids:?}"
        );
        assert!(!home.exists(), "group={group}: {home:?} is gone");
        assert_eq!(
            std::fs::read(&scratch.config).expect("the config"),
            before,
            "group={group}: nothing was written"
        );
        if group {
            assert_eq!(
                logged(&scratch.log, "pgid ").first().map(String::as_str),
                Some(tcr.to_string().as_str()),
                "codex shares tcr's process group"
            );
        }
    }
}

#[test]
fn cancelling_codex_login_reports_a_throwaway_folder_it_could_not_delete() {
    let scratch = Scratch::new("[]");
    let auth = scratch.path("prepared-auth.json");
    std::fs::write(&auth, bob_auth()).unwrap();
    write_script(&scratch.codex, &format!(
        "#!/bin/sh\necho pid $$ >> '{log}'\necho \"home $CODEX_HOME\" >> '{log}'\n/bin/cp '{auth}' \"$CODEX_HOME/auth.json\"\n/bin/chmod 500 \"$CODEX_HOME\"\necho ready >> '{log}'\nwhile :; do /bin/sleep 1; done\n",
        log = scratch.log.display(), auth = auth.display(),
    ));
    let error_log = scratch.path("cancel.stderr");
    let event_log = scratch.path("cancel.stdout");
    let mut child = scratch
        .tcr()
        .args(["login", "--codex", "--non-interactive", "--config"])
        .arg(&scratch.config)
        .stdout(std::fs::File::create(&event_log).unwrap())
        .stderr(std::fs::File::create(&error_log).unwrap())
        .spawn()
        .unwrap();
    let mut cleanup = TestProcessCleanup::new(child.id());
    // Allow both freshly built tcr and the fresh shell fixture to finish loader startup.
    let ready = within(Duration::from_secs(20), || {
        std::fs::read_to_string(&scratch.log)
            .unwrap_or_default()
            .contains("ready")
    });
    for pid in logged(&scratch.log, "pid ")
        .iter()
        .filter_map(|pid| pid.parse::<u32>().ok())
    {
        cleanup.remember(pid);
    }
    if !ready {
        let _ = child.kill();
        let _ = child.wait();
        panic!(
            "fake codex never became ready: stderr={} stdout={}",
            std::fs::read_to_string(&error_log).unwrap_or_default(),
            std::fs::read_to_string(&event_log).unwrap_or_default()
        );
    }
    let folder = scratch.homes().remove(0);
    let sent = Command::new("/bin/kill")
        .args(["-s", "INT", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(sent.success());
    let mut status = None;
    let exited = within(Duration::from_secs(10), || {
        status = child.try_wait().unwrap();
        status.is_some()
    });
    if !exited {
        let _ = child.kill();
        let _ = child.wait();
    }
    let stderr = std::fs::read_to_string(&error_log).unwrap();
    std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(exited, "cancelled login did not exit");
    assert_eq!(status.unwrap().code(), Some(130));
    assert!(
        folder.join("auth.json").is_file(),
        "the failed deletion retained the fixture login"
    );
    assert!(
        stderr.contains(&format!("removing {}", folder.display()))
            && stderr.contains("delete it by hand"),
        "cleanup failure must identify the folder and recovery: {stderr}"
    );
}

/// The folder is deleted as soon as the login is read, before the import's workspace check
/// goes out on the network: a cancel during that wait finds no login on disk.
///
/// Watched red: with the folder closed at the end of the login, it still exists when the
/// check arrives.
#[test]
fn login_codex_deletes_its_folder_before_the_workspace_check() {
    let present: Arc<Mutex<Vec<bool>>> = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&present);
    let scratch = Scratch::with_upstream("[]", move |log| {
        let folder = logged(log, "home ").last().map(PathBuf::from);
        seen.lock()
            .expect("the record")
            .push(folder.is_some_and(|folder| folder.exists()));
    });
    std::fs::create_dir_all(scratch.path(".codex")).expect("~/.codex");
    std::fs::write(scratch.path(".codex/auth.json"), "").expect("codex's own login");
    scratch.fake_codex(0, &browser_lines(), Some(&bob_auth()));

    let out = scratch.login(&["--non-interactive"]);
    assert_eq!(out.status.code(), Some(0), "{}", says(&out));
    assert_eq!(events(&out), fixture()[..4], "{}", says(&out));
    assert_eq!(
        *present.lock().expect("the record"),
        [false],
        "one check, and the folder was already gone"
    );
}

/// An npm `codex` is a `#!/usr/bin/env node` launcher reached through a symlink, and `node`
/// sits beside the link. tcr runs `codex login` with that directory first on its `PATH`, so a
/// tcr whose own `PATH` holds no `node` (TcrBar started from Finder) still signs in.
///
/// Watched red: without the prepend, or with the link's target directory instead, `env` finds
/// no `fake-node` and the sign-in fails with status 127.
#[test]
fn codex_login_finds_node_beside_its_launcher() {
    let scratch = Scratch::new("[]");
    let package = scratch.path("npm/lib/node_modules/@openai/codex/bin");
    std::fs::create_dir_all(&package).expect("the package");
    write_script(&package.join("codex.js"), "#!/usr/bin/env fake-node\n");
    let bin = scratch.path("npm/bin");
    std::fs::create_dir_all(&bin).expect("npm's bin");
    std::os::unix::fs::symlink(
        "../lib/node_modules/@openai/codex/bin/codex.js",
        bin.join("codex"),
    )
    .expect("the launcher link");
    scratch.fake_codex(0, &browser_lines(), Some(&bob_auth()));
    std::fs::rename(&scratch.codex, bin.join("fake-node")).expect("the fake becomes node");

    let out = scratch.login_with(&["--non-interactive"], |command| {
        command
            .env("TCR_CODEX_BIN", bin.join("codex"))
            .env("PATH", "/usr/bin:/bin");
    });
    assert_eq!(out.status.code(), Some(0), "{}", says(&out));
    assert_eq!(
        events(&out).last().expect("a last line")["event"],
        "saved",
        "{}",
        says(&out)
    );
}

/// The same ChatGPT user signing in to another workspace than the row's is refused naming both
/// workspaces and their plans, whatever the row is named, and nothing is written.
///
/// Watched red: keyed on the row name's email, the row named `work` gets the different-user
/// refusal.
#[test]
fn a_relogin_into_another_workspace_names_both_workspaces() {
    for name in ["erin@example.com/codex", "work"] {
        let scratch = Scratch::new(&format!(
            r#"[{{ "name": "{name}", "provider": "codex", "credentialVersion": 2,
                  "accountUuid": "77777777-7777-4777-8777-777777777777",
                  "orgUuid": "88888888-8888-4888-8888-888888888888", "organizationType": "pro",
                  "accessToken": "at-erin", "refreshToken": "rt-erin",
                  "expiresAt": 1789000000000 }}]"#
        ));
        scratch.fake_codex(
            0,
            &browser_lines(),
            Some(&auth_json_for(
                "erin@example.com",
                "77777777-7777-4777-8777-777777777777",
                "99999999-9999-4999-8999-999999999999",
                "business",
            )),
        );
        let before = std::fs::read(&scratch.config).expect("the config");
        let out = scratch.login(&["--account", name, "--non-interactive"]);
        assert_eq!(out.status.code(), Some(1), "{}", says(&out));
        assert_eq!(
            events(&out).last(),
            Some(&serde_json::json!({
                "code": "codex-identity",
                "event": "error",
                "reason": format!(
                    "the ChatGPT login that came back is erin@example.com in ChatGPT workspace \
                     99999999 (ChatGPT Business), but '{name}' holds workspace 88888888 (ChatGPT \
                     Pro). Retry `tcr login --codex --account {name}` and choose that workspace \
                     when ChatGPT asks which one to use. Nothing was written."
                )
            })),
            "{}",
            says(&out)
        );
        assert_eq!(std::fs::read(&scratch.config).expect("the config"), before);
        assert_eq!(
            scratch.answered.load(Ordering::SeqCst),
            0,
            "no workspace check"
        );
    }
}

/// `tcr run --codex` finds codex by the same search: a `codex` only in a searched directory is
/// launched, and a `TCR_CODEX_BIN` that names no executable is refused, never replaced.
///
/// Watched red: resolving codex by the old variables-then-`PATH` rule finds nothing in the
/// searched directory.
#[test]
fn run_codex_uses_the_same_search() {
    let scratch = Scratch::new("[]");
    let d1 = scratch.path("d1");
    std::fs::create_dir_all(&d1).expect("a searched directory");
    let marker = scratch.path("ran");
    write_script(
        &d1.join("codex"),
        &format!("#!/bin/sh\n/usr/bin/touch '{}'\n", marker.display()),
    );
    let run = |change: &dyn Fn(&mut Command)| {
        let mut command = scratch.tcr();
        command
            .args(["run", "--codex", "--config"])
            .arg(&scratch.config)
            .env_remove("TCR_CODEX_BIN")
            .env_remove("CODEX_BIN")
            .env("TCR_CODEX_SEARCH_DIRS", &d1);
        change(&mut command);
        command.output().expect("spawning the built tcr binary")
    };
    let out = run(&|_| {});
    assert_eq!(out.status.code(), Some(0), "{}", says(&out));
    assert!(marker.exists(), "the searched codex ran");
    std::fs::remove_file(&marker).expect("resetting the marker");

    let plain = scratch.path("plain");
    std::fs::write(&plain, "not a program").expect("a plain file");
    std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).expect("0644");
    let out = run(&|command| {
        command.env("TCR_CODEX_BIN", &plain);
    });
    assert_eq!(out.status.code(), Some(1), "{}", says(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains(&format!(
            "TCR_CODEX_BIN is {}, which is not an executable file.",
            plain.display()
        )),
        "{}",
        says(&out)
    );
    assert!(!marker.exists(), "nothing ran");
}

/// `tcr login --codex --account` naming a Claude row is refused before codex runs.
#[test]
fn login_codex_account_on_a_claude_row_runs_no_codex() {
    let scratch = Scratch::new(r#"[{ "name": "alice@example.com", "accessToken": "at-alice" }]"#);
    scratch.fake_codex(0, &browser_lines(), Some(&bob_auth()));
    let out = scratch.login(&["--account", "alice@example.com", "--non-interactive"]);
    assert_eq!(out.status.code(), Some(1), "{}", says(&out));
    assert_eq!(
        events(&out),
        [serde_json::json!({
            "code": "account-is-claude",
            "event": "error",
            "reason": "'alice@example.com' is a Claude account; `tcr login --codex` signs in \
                       Codex accounts only. Sign it in again with `tcr login --account \
                       alice@example.com`. Nothing was written."
        })]
    );
    assert!(scratch.homes().is_empty(), "codex never ran");
}

/// A stub of a running proxy's account-add route that names no providers, the shape of every
/// proxy built before Codex accounts: it answers the blank probe with the route's stamped 400.
/// Returns its port.
fn proxy_without_providers() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("binding the stub proxy");
    let port = listener.local_addr().expect("an address").port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(&mut stream);
            let mut length = 0usize;
            let mut line = String::new();
            while reader.read_line(&mut line).is_ok_and(|n| n > 0) && line != "\r\n" {
                if let Some((name, value)) = line.split_once(':') {
                    if name.eq_ignore_ascii_case("content-length") {
                        length = value.trim().parse().unwrap_or(0);
                    }
                }
                line.clear();
            }
            let mut body = vec![0u8; length];
            let _ = reader.read_exact(&mut body);
            let refusal = r#"{"type":"error","error":{"type":"invalid_request_error","message":"\"name\" (string) is required and must not be empty."}}"#;
            let _ = write!(
                stream,
                "HTTP/1.1 400 Bad Request\r\n{}: {}\r\ncontent-type: application/json\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n{refusal}",
                tcr::proxy::ENDPOINT_HEADER,
                tcr::proxy::ADD_ACCOUNT_ENDPOINT,
                refusal.len()
            );
        }
    });
    port
}

/// A running proxy that cannot store a Codex row refuses `tcr login --codex` before codex runs,
/// naming that command as the one to run again, and nothing is written: the person never
/// spends a browser sign-in on a login the proxy would file as a Claude account.
///
/// Watched red: with the provider gate left to the import, codex runs first.
#[test]
fn login_codex_is_refused_by_a_live_proxy_that_names_no_providers() {
    let scratch = Scratch::new("[]");
    let port = proxy_without_providers();
    let text = std::fs::read_to_string(&scratch.config).expect("the config");
    let mut doc: serde_json::Value = serde_json::from_str(&text).expect("JSON");
    doc["proxy"]["port"] = serde_json::json!(port);
    std::fs::write(&scratch.config, doc.to_string()).expect("pointing at the stub");
    let before = std::fs::read(&scratch.config).expect("the config");
    scratch.fake_codex(0, &browser_lines(), Some(&bob_auth()));

    let out = scratch.login(&["--non-interactive"]);
    assert_eq!(out.status.code(), Some(1), "{}", says(&out));
    let lines = events(&out);
    assert_eq!(lines.len(), 1, "{}", says(&out));
    let reason = lines[0]["reason"].as_str().unwrap_or_default();
    assert!(
        reason.contains("predates Codex accounts")
            && reason.contains("then re-run `tcr login --codex`"),
        "{reason}"
    );
    assert!(scratch.homes().is_empty(), "codex never ran");
    assert_eq!(std::fs::read(&scratch.config).expect("the config"), before);
}

#[test]
fn a_failed_codex_login_reports_its_folder_cleanup_failure_too() {
    let scratch = Scratch::new("[]");
    let auth = scratch.path("prepared-auth.json");
    std::fs::write(&auth, bob_auth()).unwrap();
    write_script(&scratch.codex, &format!(
        "#!/bin/sh\necho \"home $CODEX_HOME\" >> '{log}'\n/bin/cp '{auth}' \"$CODEX_HOME/auth.json\"\n/bin/chmod 500 \"$CODEX_HOME\"\nexit 7\n",
        log=scratch.log.display(), auth=auth.display(),
    ));
    let out = scratch
        .tcr()
        .args(["login", "--codex", "--non-interactive", "--config"])
        .arg(&scratch.config)
        .output()
        .unwrap();
    let folder = scratch.homes().remove(0);
    std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o700)).unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(
        stderr.contains("exited with 7")
            && stderr.contains("removing")
            && stderr.contains("delete it by hand"),
        "the sign-in and cleanup failures must both be reported: {stderr}"
    );
    assert_eq!(events(&out)[0]["code"], "codex-login-failed");
}
