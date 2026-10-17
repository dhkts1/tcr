//! `tcr doctor` end to end, against a scratch HOME and a fake status endpoint.
//!
//! This tests the BUILT BINARY rather than the functions in `src/doctor.rs`
//! (which carry their own unit tests) because the whole verb is its exit code
//! and its printed lines: a script asks `tcr doctor >/dev/null || open the
//! settings file`, and the panel will shell out to `tcr doctor --json`. Neither
//! contract exists inside a library function.
//!
//! # Isolation
//!
//! Same rules as `tests/first_run_no_config.rs`: `HOME` points at a fresh
//! [`tempfile::TempDir`], the config is written into that temp tree and passed
//! explicitly, and the port under test is a kernel-assigned one bound by this
//! test process. Nothing here reads the operator's real
//! `.config/teamclaude.json`, and no test here names a port literal at all:
//! every port comes from `TcpListener::bind("127.0.0.1:0")`, so the live proxy
//! is never the thing a test connects to.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;

/// One Claude account, fake from end to end. Its token expires in 2100, and `tcr doctor` never
/// refreshes, probes or rotates, so the row is only ever read.
const ALICE: &str = r#"{ "name": "alice@example.com", "accessToken": "at-alice", "refreshToken": "rt-alice", "expiresAt": 4102444800000 }"#;

/// A port nothing is listening on: bound from the kernel's ephemeral range and
/// released immediately.
///
/// Never a literal. The live proxy on this machine serves on the configured
/// default port, and a test that wrote that number would have `tcr status` and
/// `tcr doctor` talk to it.
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a scratch port");
    listener.local_addr().expect("local addr").port()
}

/// A config with no accounts and one port. `tcr doctor` never rotates, probes
/// or refreshes, so an empty fleet is the honest fixture: the verb's subject is
/// the ROUTE, not the accounts.
fn write_config(home: &Path, port: u16) -> std::path::PathBuf {
    let dir = home.join(".config");
    std::fs::create_dir_all(&dir).expect("create the config directory");
    let path = dir.join("teamclaude.json");
    std::fs::write(
        &path,
        format!("{{\n  \"accounts\": [],\n  \"proxy\": {{ \"port\": {port} }}\n}}\n"),
    )
    .expect("write the config");
    path
}

/// [`write_config`]'s file, holding `accounts` (a comma-separated list of account objects)
/// instead of none.
fn write_config_with(home: &Path, port: u16, accounts: &str) -> PathBuf {
    let dir = home.join(".config");
    std::fs::create_dir_all(&dir).expect("create the config directory");
    let path = dir.join("teamclaude.json");
    std::fs::write(
        &path,
        format!("{{\n  \"accounts\": [{accounts}],\n  \"proxy\": {{ \"port\": {port} }}\n}}\n"),
    )
    .expect("write the config");
    path
}

/// `text` with every fact that changes from run to run replaced by a placeholder: the scratch
/// HOME (as written, and in the canonical form the kernel resolves it to, `/var` becoming
/// `/private/var`) by `<home>`, the scratch port by `<port>`, and a port holder's pid and name by
/// `<pid> <holder>`. The holder of an answering port is this test binary, whose name carries a
/// build hash.
fn normalize(text: &str, home: &Path, port: u16) -> String {
    let mut out = text.to_string();
    if let Ok(canonical) = std::fs::canonicalize(home) {
        out = out.replace(&canonical.display().to_string(), "<home>");
    }
    out = out
        .replace(&home.display().to_string(), "<home>")
        .replace(&format!("127.0.0.1:{port}"), "127.0.0.1:<port>")
        .replace(&format!(":{port} "), ":<port> ")
        .replace(&format!("proxyPort: {port}"), "proxyPort: <port>")
        .replace(&format!("\"proxyPort\": {port}"), "\"proxyPort\": <port>");
    out.split_inclusive('\n')
        .map(|line| {
            if line.starts_with("portHolder: pid ") {
                let end = if line.ends_with('\n') { "\n" } else { "" };
                format!("portHolder: pid <pid> <holder>{end}")
            } else {
                line.to_string()
            }
        })
        .collect()
}

fn write_settings(home: &Path, base_url: &str) -> std::path::PathBuf {
    let dir = home.join(".claude");
    std::fs::create_dir_all(&dir).expect("create the settings directory");
    let path = dir.join("settings.json");
    std::fs::write(
        &path,
        format!("{{\n  \"env\": {{ \"ANTHROPIC_BASE_URL\": \"{base_url}\" }}\n}}\n"),
    )
    .expect("write settings.json");
    path
}

/// Run `tcr <args…>` with a scratch `HOME`, a scratch working directory of its
/// own, and `ANTHROPIC_BASE_URL` set or cleared explicitly.
///
/// The working directory is a `work` subdirectory rather than the scratch HOME
/// itself, and that is load-bearing twice over. It keeps the REAL checkout's
/// `.claude` directory from being read as this test's project settings; and it
/// keeps `<cwd>/.claude/settings.json` from BEING `<home>/.claude/settings.json`,
/// which made a test written to exercise the user settings silently exercise the
/// project ones instead (and report the kernel's canonical `/private/var` path
/// for them, since `current_dir` resolves symlinks and `HOME` does not).
///
/// Both variables are set on the spawned `Command`, never on this test process:
/// a test that exported one into its own environment would leak it into every
/// other test in this binary.
/// A working directory for the child, inside the scratch HOME and holding no
/// `.claude` of its own.
fn work_dir(home: &Path) -> std::path::PathBuf {
    let path = home.join("work");
    std::fs::create_dir_all(&path).expect("create the scratch working directory");
    path
}

fn run_tcr(
    home: &Path,
    base_url_env: Option<&str>,
    args: &[&str],
) -> (Option<i32>, String, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tcr"));
    command
        .args(args)
        .env("HOME", home)
        .env_remove("XDG_CACHE_HOME")
        // doctor looks for `tcr setup shell`'s block in the shell's startup
        // files; neither variable may point that read outside the scratch HOME.
        .env_remove("ZDOTDIR")
        .env_remove("XDG_CONFIG_HOME")
        // A scratch HOME does not scratch the login Keychain, see
        // `tests/first_run_no_config.rs` for why this override is set to a path
        // that does not exist.
        .env(
            "TCR_CLAUDE_CODE_CREDENTIALS",
            home.join("no-credentials.json"),
        )
        .current_dir(work_dir(home));
    match base_url_env {
        Some(url) => command.env("ANTHROPIC_BASE_URL", url),
        None => command.env_remove("ANTHROPIC_BASE_URL"),
    };
    let out = command.output().expect("run tcr");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A `/_tcr/status` endpoint on a kernel-assigned port, answering every
/// connection with a payload carrying `served` requests in the newest minute of
/// one session's sparkline. Returns the port it bound.
///
/// Hand-written HTTP rather than the real server: standing up
/// `server::serve` would need accounts, a manager and a bind of its own, and
/// what is under test is the CLIENT's reading of a payload. The bytes are the
/// contract (`crate::status::STATUS_KIND`), so they are written out here
/// literally, so a change to that contract must fail this test loudly.
///
/// The accept loop never ends and the handle is dropped on purpose. An earlier
/// version served exactly one connection and then returned, which dropped the
/// listener the instant `tcr doctor` finished reading it: the verb then
/// enumerated the port, found nobody holding it, and printed `portHolder:
/// none` about a socket that had been open a millisecond earlier. The thread
/// dies with the test binary.
fn fake_status_endpoint(served: u16) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a scratch port");
    let port = listener.local_addr().expect("local addr").port();
    let mut sparkline = vec![0u16; 30];
    sparkline[29] = served;
    let body = format!(
        "{{\"kind\":\"tcr.status.v1\",\"accounts\":[],\"sessions\":[{{\"sessionId\":\"11111111-1111-1111-1111-111111111111\",\"firstSeenMs\":0,\"lastSeenMs\":0,\"reqPerMinute\":{sparkline:?}}}]}}"
    );
    std::thread::spawn(move || {
        loop {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            serve_one(stream, &body);
        }
    });
    port
}

fn serve_one(mut stream: TcpStream, body: &str) {
    // Drain the request head so the client's write completes before we answer.
    let peek = stream.try_clone().expect("clone the stream");
    let mut reader = BufReader::new(peek);
    let mut line = String::new();
    while reader.read_line(&mut line).is_ok_and(|read| read > 0) {
        if line == "\r\n" || line == "\n" {
            break;
        }
        line.clear();
    }
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _written = stream.write_all(response.as_bytes());
    let _flushed = stream.flush();
}

/// A prior failure, reproduced: a settings file points Claude Code at
/// another gateway, so the proxy is healthy and serving nobody. `doctor` must
/// exit 2 and name the FILE: the operator's next action is to open it.
#[test]
fn a_settings_file_pointing_elsewhere_exits_two_and_names_the_file() {
    let home = tempfile::tempdir().expect("temp home");
    let settings = write_settings(home.path(), "https://gateway.example.com");
    let port = free_port();
    let config = write_config(home.path(), port);

    let (code, stdout, stderr) = run_tcr(
        home.path(),
        Some(&format!("http://127.0.0.1:{port}")),
        &["doctor", "--config", &config.display().to_string()],
    );

    assert_eq!(code, Some(2), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(
        stdout.contains(&format!("baseUrlSource: {}", settings.display())),
        "the source line must name the settings file:\n{stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "verdict: this proxy is not on Claude's route: {} sets https://gateway.example.com",
            settings.display()
        )),
        "the verdict must name the file and the URL it sets:\n{stdout}"
    );
    assert!(
        stdout.contains("baseUrl: https://gateway.example.com"),
        "{stdout}"
    );
}

/// The healthy case: nothing in the scratch HOME overrides the route, the
/// environment points at a proxy on this machine, and that proxy answers.
#[test]
fn a_route_to_an_answering_proxy_exits_zero_and_reports_the_requests() {
    let home = tempfile::tempdir().expect("temp home");
    let port = fake_status_endpoint(3);
    let config = write_config(home.path(), port);

    let (code, stdout, stderr) = run_tcr(
        home.path(),
        Some(&format!("http://127.0.0.1:{port}")),
        &["doctor", "--config", &config.display().to_string()],
    );

    assert_eq!(code, Some(0), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("requests10m: 3"), "{stdout}");
    assert!(
        stdout.contains(&format!(
            "verdict: Claude is routed to http://127.0.0.1:{port} (ANTHROPIC_BASE_URL); this proxy has served 3 requests"
        )),
        "{stdout}"
    );
    // The port holder is this test process, which is not a tcr, and doctor must
    // say so rather than assume the listener it found is ours.
    assert!(stdout.contains("portHolderIsTcr: no"), "{stdout}");
    assert!(
        stdout.contains(&format!("portHolder: pid {}", std::process::id())),
        "the holder must be named by pid, and it is this test process:\n{stdout}"
    );
}

/// Routed here, nothing listening: exit 3, and the request count reads `none`
/// rather than a fabricated zero.
#[test]
fn a_route_here_with_no_proxy_exits_three() {
    let home = tempfile::tempdir().expect("temp home");
    let port = free_port();
    let config = write_config(home.path(), port);

    let (code, stdout, stderr) = run_tcr(
        home.path(),
        Some(&format!("http://127.0.0.1:{port}")),
        &["doctor", "--config", &config.display().to_string()],
    );

    assert_eq!(code, Some(3), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("requests10m: none"), "{stdout}");
    assert!(stdout.contains("portHolder: none"), "{stdout}");
    assert!(
        stdout.contains("no proxy answered on 127.0.0.1:"),
        "{stdout}"
    );
}

/// `--json` carries the same decision as one object, including the exit code,
/// so the panel reads one document rather than parsing lines.
#[test]
fn the_json_form_carries_the_verdict_and_the_exit_code() {
    let home = tempfile::tempdir().expect("temp home");
    write_settings(home.path(), "https://gateway.example.com");
    let config = write_config(home.path(), free_port());

    let (code, stdout, stderr) = run_tcr(
        home.path(),
        None,
        &[
            "doctor",
            "--json",
            "--config",
            &config.display().to_string(),
        ],
    );

    assert_eq!(code, Some(2), "stdout:\n{stdout}\nstderr:\n{stderr}");
    let document: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("--json must emit one JSON object ({e}):\n{stdout}"));
    assert_eq!(document["exitCode"], 2);
    assert_eq!(document["routedHere"], false);
    assert_eq!(document["baseUrl"], "https://gateway.example.com");
    assert_eq!(document["baseUrlSource"]["kind"], "userSettings");
    assert_eq!(document["requestsInWindow"], serde_json::Value::Null);
    assert!(document["verdict"].as_str().is_some_and(|verdict| {
        verdict.starts_with("verdict: this proxy is not on Claude's route")
    }));
}

/// A `tcr status` glance shows the route problem, first
/// line, so nobody has to know `doctor` exists to be told the proxy they are
/// reading is not the one their Claude talks to.
#[test]
fn tcr_status_leads_with_the_verdict_when_the_route_is_elsewhere() {
    let home = tempfile::tempdir().expect("temp home");
    let settings = write_settings(home.path(), "https://gateway.example.com");
    let config = write_config(home.path(), free_port());

    let (code, stdout, stderr) = run_tcr(
        home.path(),
        None,
        &["status", "--config", &config.display().to_string()],
    );

    assert_eq!(code, Some(0), "status still exits 0:\n{stdout}\n{stderr}");
    let first = stdout.lines().next().unwrap_or_default();
    assert_eq!(
        first,
        format!(
            "verdict: this proxy is not on Claude's route: {} sets https://gateway.example.com",
            settings.display()
        ),
        "the verdict must be the FIRST line of `tcr status`:\n{stdout}"
    );
    assert!(
        stdout.contains("status source="),
        "the fleet view still follows it:\n{stdout}"
    );
}

/// The `--json` fleet contract is a bare array, and the verdict line must never
/// appear in it: `tcr status --json | jq` is what the panel runs.
#[test]
fn tcr_status_json_stays_a_bare_array() {
    let home = tempfile::tempdir().expect("temp home");
    write_settings(home.path(), "https://gateway.example.com");
    let config = write_config(home.path(), free_port());

    let (code, stdout, stderr) = run_tcr(
        home.path(),
        None,
        &[
            "status",
            "--json",
            "--config",
            &config.display().to_string(),
        ],
    );

    assert_eq!(code, Some(0), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(
        !stdout.contains("verdict:"),
        "the json form carries no verdict line:\n{stdout}"
    );
    let document: serde_json::Value =
        serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("bare array ({e}):\n{stdout}"));
    assert!(document.is_array(), "{stdout}");
}

/// Where [`claude_only_doctor_is_unchanged`] keeps the bytes it pins.
fn claude_only_golden_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/doctor-claude-only.golden")
}

/// What `tcr doctor` prints for a fleet of Claude accounts alone, pinned before Codex accounts
/// existed in it: a Codex block, a Codex verdict and a Codex exit code may only ever appear when
/// the fleet holds a Codex account, or with `--codex`. Four runs, each on a fresh scratch HOME
/// whose config holds one Claude account: Claude sent elsewhere by a settings file, routed here
/// with a proxy answering, routed here with nothing answering, and the first again as JSON.
///
/// The golden was written once, by `TCR_UPDATE_DOCTOR_GOLDEN=1`, from the code as it stood
/// before Codex accounts reached `tcr doctor`. It is never regenerated to fit a change.
#[test]
fn claude_only_doctor_is_unchanged() {
    // (label, a settings file sends Claude elsewhere, a proxy answers, `--json`)
    let scenarios = [
        ("a settings file sends Claude elsewhere", true, false, false),
        ("routed here, the proxy answers", false, true, false),
        ("routed here, nothing answers", false, false, false),
        (
            "json, a settings file sends Claude elsewhere",
            true,
            false,
            true,
        ),
    ];
    let mut text = String::new();
    for (label, settings, answers, json) in scenarios {
        let home = tempfile::tempdir().expect("temp home");
        if settings {
            write_settings(home.path(), "https://gateway.example.com");
        }
        let port = if answers {
            fake_status_endpoint(3)
        } else {
            free_port()
        };
        let config = write_config_with(home.path(), port, ALICE)
            .display()
            .to_string();
        let mut args = vec!["doctor"];
        if json {
            args.push("--json");
        }
        args.extend(["--config", &config]);
        let (code, stdout, stderr) = run_tcr(
            home.path(),
            Some(&format!("http://127.0.0.1:{port}")),
            &args,
        );
        text.push_str(&normalize(
            &format!("== {label}\nexit: {code:?}\n-- stdout\n{stdout}-- stderr\n{stderr}"),
            home.path(),
            port,
        ));
    }

    let golden = claude_only_golden_path();
    if std::env::var_os("TCR_UPDATE_DOCTOR_GOLDEN").is_some() {
        std::fs::write(&golden, &text).expect("write the golden");
        return;
    }
    let pinned = std::fs::read_to_string(&golden)
        .unwrap_or_else(|e| panic!("cannot read the golden at {} ({e})", golden.display()));
    assert_eq!(
        text,
        pinned,
        "`tcr doctor` no longer prints what a Claude-only fleet saw before Codex accounts reached \
         it ({}). This golden is never regenerated to fit a change, TCR_UPDATE_DOCTOR_GOLDEN \
         included: a fleet with no Codex account must read exactly as it did.",
        golden.display()
    );
}

/// A `/_tcr/status` endpoint on a kernel-assigned port, answering every connection with
/// `body`, as [`fake_status_endpoint`] does. `body` must parse as the payload the CLI reads, so
/// a misspelled key fails here, loudly, rather than as a status the CLI could not read.
fn serve_status(body: serde_json::Value) -> u16 {
    if let Err(error) = serde_json::from_value::<tcr::status::StatusPayload>(body.clone()) {
        panic!("the fake status payload does not parse ({error}): {body}");
    }
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a scratch port");
    let port = listener.local_addr().expect("local addr").port();
    let body = body.to_string();
    std::thread::spawn(move || {
        loop {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            serve_one(stream, &body);
        }
    });
    port
}

/// One account row of a status payload: active, probed, in rotation, nothing served.
fn status_row(name: &str, codex: bool) -> serde_json::Value {
    let mut row = serde_json::json!({
        "name": name, "priority": 0, "status": "active", "disabled": false, "requests": 0,
        "inputTokens": 0, "outputTokens": 0, "cacheReadTokens": 0, "cacheCreationTokens": 0,
        "probeStatus": "ok", "quotaState": "normal", "gate": "ok", "threshold": 0.9,
    });
    if codex {
        row["provider"] = serde_json::json!("codex");
    }
    row
}

/// One session of a status payload, [`fake_status_endpoint`]'s, with `served` requests in its
/// newest minute.
fn session_row(id: &str, served: u16, codex: bool) -> serde_json::Value {
    let mut sparkline = vec![0u16; 30];
    sparkline[29] = served;
    let mut row = serde_json::json!({
        "sessionId": id, "firstSeenMs": 0, "lastSeenMs": 0, "reqPerMinute": sparkline,
    });
    if codex {
        row["provider"] = serde_json::json!("codex");
    }
    row
}

/// A status payload holding `rows` and `sessions`, whose MITM listener serves `leaf` and
/// intercepts `hosts`.
fn payload(
    rows: Vec<serde_json::Value>,
    sessions: Vec<serde_json::Value>,
    leaf: &str,
    hosts: &[&str],
) -> serde_json::Value {
    serde_json::json!({
        "kind": "tcr.status.v1",
        "accounts": rows,
        "sessions": sessions,
        "mitm": {"interceptedHosts": hosts, "leaf": leaf},
    })
}

/// [`run_tcr`], with this machine's codex kept out: no proxy or CA variable reaches the child,
/// codex's login can only be `<home>/.codex/auth.json`, and the only `codex` it can find is a
/// fake one at `<home>/bin/codex`, named by `TCR_CODEX_BIN`, with the search past `PATH`
/// pointed at an empty folder, so no real codex and no npm prefix is ever consulted.
fn run_tcr_codex(
    home: &Path,
    base_url_env: Option<&str>,
    args: &[&str],
) -> (Option<i32>, String, String) {
    use std::os::unix::fs::PermissionsExt as _;
    let bin = home.join("bin");
    std::fs::create_dir_all(&bin).expect("create the scratch bin directory");
    let codex = bin.join("codex");
    std::fs::write(&codex, "#!/bin/sh\nexit 0\n").expect("write the fake codex");
    std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o755))
        .expect("make the fake codex executable");
    let no_dirs = home.join("no-dirs");
    std::fs::create_dir_all(&no_dirs).expect("create the empty search directory");

    let mut command = Command::new(env!("CARGO_BIN_EXE_tcr"));
    command
        .args(args)
        .env("HOME", home)
        .env_remove("XDG_CACHE_HOME")
        // doctor looks for `tcr setup shell`'s block in the shell's startup
        // files; neither variable may point that read outside the scratch HOME.
        .env_remove("ZDOTDIR")
        .env_remove("XDG_CONFIG_HOME")
        .env(
            "TCR_CLAUDE_CODE_CREDENTIALS",
            home.join("no-credentials.json"),
        )
        .current_dir(work_dir(home));
    for var in [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
        "NO_PROXY",
        "no_proxy",
        "SSL_CERT_FILE",
        "CODEX_CA_CERTIFICATE",
        "CODEX_HOME",
        "TCR_CODEX_AUTH",
        "CODEX_BIN",
    ] {
        command.env_remove(var);
    }
    command
        .env("TCR_CODEX_BIN", &codex)
        .env("TCR_CODEX_SEARCH_DIRS", &no_dirs);
    match base_url_env {
        Some(url) => command.env("ANTHROPIC_BASE_URL", url),
        None => command.env_remove("ANTHROPIC_BASE_URL"),
    };
    let out = command.output().expect("run tcr");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A port nothing listens on that is not `excluded`: the kernel can hand a released port
/// straight back, and a run meant to find no proxy must not meet one.
fn free_port_other_than(excluded: u16) -> u16 {
    for _ in 0..20 {
        let port = free_port();
        if port != excluded {
            return port;
        }
    }
    panic!("the kernel handed back {excluded} twenty times in a row");
}

/// The mixed fleet of the tests below: alice for Claude, bob for Codex, each with a session
/// (3 Claude requests and 2 Codex ones in the newest minute), behind a proxy that runs without
/// MITM and has refused 4 Codex requests and relayed 1 unpooled.
fn mixed_payload() -> serde_json::Value {
    let mut body = payload(
        vec![
            status_row("alice@example.com", false),
            status_row("bob@example.com/codex", true),
        ],
        vec![
            session_row("11111111-1111-1111-1111-111111111111", 3, false),
            session_row("22222222-2222-2222-2222-222222222222", 2, true),
        ],
        "none",
        &[],
    );
    body["codex_traffic"] = serde_json::json!({"refused10m": 4, "unpooled10m": 1});
    body
}

/// A mixed fleet's doctor prints Claude's facts, the Codex block and both verdicts, Claude's
/// last and in words that say its count is Claude's, and exits with Claude's code: a script
/// asking whether Claude Code reaches this proxy gets the answer it always got, even while
/// codex is not pooled.
#[test]
fn a_mixed_fleet_doctor_keeps_claudes_exit_code() {
    let home = tempfile::tempdir().expect("temp home");
    let port = serve_status(mixed_payload());
    let config = write_config_with(home.path(), port, ALICE)
        .display()
        .to_string();

    let (code, stdout, stderr) = run_tcr_codex(
        home.path(),
        Some(&format!("http://127.0.0.1:{port}")),
        &["doctor", "--config", &config],
    );

    assert_eq!(code, Some(0), "stdout:\n{stdout}\nstderr:\n{stderr}");
    let text = normalize(&stdout, home.path(), port);
    let tail: Vec<&str> = text.lines().skip(5).collect();
    assert_eq!(
        tail,
        [
            "requests10m: 3",
            "shellRoute: not installed",
            "codexAccounts: 1 (1 in rotation)",
            "codexIntercept: no",
            "codexCa: <home>/.config/tcr-ca.pem (not readable)",
            "codexUpstream: https://chatgpt.com (default)",
            "codexBin: <home>/bin/codex (TCR_CODEX_BIN)",
            "codexEnv: HTTPS_PROXY unset · NO_PROXY unset · SSL_CERT_FILE unset",
            "codexLogin: <home>/.codex/auth.json does not exist (existence only; never read)",
            "codexRequests10m: 2",
            "codexRefused10m: 4",
            "codexUnpooled10m: 1",
            "codexProblem: auth.json is absent; a keyring-backed Codex login is not checked. Run \
             codex once to verify its own sign-in before using the pool",
            "codexProblem: the proxy on :<port> is running without MITM: its TLS material failed \
             to load at boot — see its log",
            "codexVerdict: codex is not pooled: the proxy on :<port> is running without MITM: \
             its TLS material failed to load at boot — see its log",
            "verdict: Claude is routed to http://127.0.0.1:<port> (ANTHROPIC_BASE_URL); this \
             proxy has served 3 Claude requests",
        ],
        "{text}"
    );
}

/// With no Claude account the Codex verdict is THE verdict: keyed `verdict:`, last, and its
/// code is the exit code, whatever Claude's route would have said.
#[test]
fn a_codex_only_doctor_ends_with_the_codex_verdict() {
    // Not pooled: Claude's own route is fine (it would exit 0); codex's is not.
    let home = tempfile::tempdir().expect("temp home");
    let port = serve_status(payload(
        vec![status_row("bob@example.com/codex", true)],
        Vec::new(),
        "none",
        &[],
    ));
    let config = write_config_with(home.path(), port, ALICE)
        .display()
        .to_string();
    let (code, stdout, stderr) = run_tcr_codex(
        home.path(),
        Some(&format!("http://127.0.0.1:{port}")),
        &["doctor", "--config", &config],
    );
    assert_eq!(code, Some(2), "stdout:\n{stdout}\nstderr:\n{stderr}");
    let text = normalize(&stdout, home.path(), port);
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines[0].starts_with("baseUrl: "), "{text}");
    assert_eq!(
        lines.last().copied(),
        Some(
            "verdict: codex is not pooled: the proxy on :<port> is running without MITM: its \
             TLS material failed to load at boot — see its log"
        ),
        "{text}"
    );
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.starts_with("verdict:"))
            .count(),
        1,
        "{text}"
    );
    assert!(
        !lines.iter().any(|line| line.starts_with("codexVerdict:")),
        "{text}"
    );

    // Pooled: Claude's own route is elsewhere (it would exit 2); codex is pooled.
    let home = tempfile::tempdir().expect("temp home");
    let port = serve_status(payload(
        vec![status_row("bob@example.com/codex", true)],
        vec![session_row("22222222-2222-2222-2222-222222222222", 2, true)],
        "minted",
        &["api.anthropic.com", "platform.claude.com", "chatgpt.com"],
    ));
    let config = write_config_with(home.path(), port, ALICE)
        .display()
        .to_string();
    std::fs::write(home.path().join(".config").join("tcr-ca.pem"), "any bytes")
        .expect("write a CA file; only its presence is read");
    let (code, stdout, stderr) = run_tcr_codex(home.path(), None, &["doctor", "--config", &config]);
    assert_eq!(code, Some(0), "stdout:\n{stdout}\nstderr:\n{stderr}");
    let text = normalize(&stdout, home.path(), port);
    assert!(text.contains("\ncodexIntercept: yes\n"), "{text}");
    assert!(
        text.contains("\ncodexCa: <home>/.config/tcr-ca.pem (readable)\n"),
        "{text}"
    );
    assert_eq!(
        text.lines().last(),
        Some(
            "verdict: codex is pooled: `tcr run --codex` routes chatgpt.com through this proxy, \
             which served 2 Codex requests in the last 10 min"
        ),
        "{text}"
    );
}

/// `--codex` asks only whether codex is pooled: the Codex block and its verdict, keyed
/// `verdict:`, and Codex's exit code, even for a fleet with no Codex account and a Claude route
/// that is fine.
#[test]
fn doctor_codex_prints_only_the_codex_block_and_exits_with_its_code() {
    let home = tempfile::tempdir().expect("temp home");
    let port = serve_status(payload(
        vec![status_row("alice@example.com", false)],
        Vec::new(),
        "minted",
        &["api.anthropic.com", "platform.claude.com", "chatgpt.com"],
    ));
    let config = write_config_with(home.path(), port, ALICE)
        .display()
        .to_string();
    std::fs::write(home.path().join(".config").join("tcr-ca.pem"), "any bytes")
        .expect("write a CA file; only its presence is read");
    let (code, stdout, stderr) = run_tcr_codex(
        home.path(),
        Some(&format!("http://127.0.0.1:{port}")),
        &["doctor", "--codex", "--config", &config],
    );
    assert_eq!(code, Some(2), "stdout:\n{stdout}\nstderr:\n{stderr}");
    let text = normalize(&stdout, home.path(), port);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "codexAccounts: 0", "{text}");
    assert!(
        !lines
            .iter()
            .any(|line| line.starts_with("baseUrl:") || line.starts_with("codexVerdict:")),
        "{text}"
    );
    assert_eq!(
        lines.last().copied(),
        Some(
            "verdict: codex is not pooled: the proxy on :<port> holds no Codex account, so it \
             blind-tunnels chatgpt.com and codex keeps its own login — add one with `tcr login \
             --codex`"
        ),
        "{text}"
    );

    let offline = free_port_other_than(port);
    let config = write_config_with(home.path(), offline, ALICE)
        .display()
        .to_string();
    let (code, stdout, stderr) = run_tcr_codex(
        home.path(),
        Some(&format!("http://127.0.0.1:{offline}")),
        &["doctor", "--codex", "--config", &config],
    );
    assert_eq!(code, Some(3), "stdout:\n{stdout}\nstderr:\n{stderr}");
    let text = normalize(&stdout, home.path(), offline);
    assert!(text.contains("\ncodexIntercept: unknown\n"), "{text}");
    assert_eq!(
        text.lines().last(),
        Some("verdict: codex is not pooled: no proxy answered on 127.0.0.1:<port>"),
        "{text}"
    );
}

/// `--json` carries the Codex block as one `codex` object, and only when the text prints the
/// block: a Claude-only fleet's document is the one it always was. A mixed fleet keeps
/// Claude's `verdict` and `exitCode` at the top; `--codex --json` is the `codex` object alone.
#[test]
fn doctor_json_carries_a_codex_object_only_with_the_block() {
    // (a) A Claude-only fleet.
    let home = tempfile::tempdir().expect("temp home");
    let port = serve_status(payload(
        vec![status_row("alice@example.com", false)],
        Vec::new(),
        "minted",
        &["api.anthropic.com", "platform.claude.com", "chatgpt.com"],
    ));
    let config = write_config_with(home.path(), port, ALICE)
        .display()
        .to_string();
    let routed = format!("http://127.0.0.1:{port}");
    let (code, stdout, stderr) = run_tcr_codex(
        home.path(),
        Some(&routed),
        &["doctor", "--json", "--config", &config],
    );
    assert_eq!(code, Some(0), "stdout:\n{stdout}\nstderr:\n{stderr}");
    let document: serde_json::Value = serde_json::from_str(&stdout).expect("one JSON object");
    assert!(document.get("codex").is_none(), "{document}");

    // (b) The mixed fleet.
    let home = tempfile::tempdir().expect("temp home");
    let port = serve_status(mixed_payload());
    let config = write_config_with(home.path(), port, ALICE)
        .display()
        .to_string();
    let routed = format!("http://127.0.0.1:{port}");
    let (_, text, _) = run_tcr_codex(home.path(), Some(&routed), &["doctor", "--config", &config]);
    let text = normalize(&text, home.path(), port);
    let (code, stdout, stderr) = run_tcr_codex(
        home.path(),
        Some(&routed),
        &["doctor", "--json", "--config", &config],
    );
    assert_eq!(code, Some(0), "stdout:\n{stdout}\nstderr:\n{stderr}");
    let document: serde_json::Value = serde_json::from_str(&stdout).expect("one JSON object");
    assert_eq!(document["exitCode"], 0);
    let verdict = normalize(
        document["verdict"].as_str().expect("a verdict"),
        home.path(),
        port,
    );
    assert_eq!(
        Some(verdict.as_str()),
        text.lines().last(),
        "the JSON verdict is the text's last line"
    );
    assert_eq!(
        verdict,
        "verdict: Claude is routed to http://127.0.0.1:<port> (ANTHROPIC_BASE_URL); this proxy \
         has served 3 Claude requests"
    );
    let codex = document["codex"].as_object().expect("a codex object");
    let mut keys: Vec<&str> = codex.keys().map(String::as_str).collect();
    keys.sort_unstable();
    let mut expected = vec![
        "accounts",
        "intercept",
        "route",
        "ca",
        "upstream",
        "bin",
        "env",
        "login",
        "requests10m",
        "refused10m",
        "unpooled10m",
        "problems",
        "verdict",
        "exitCode",
    ];
    expected.sort_unstable();
    assert_eq!(keys, expected);
    assert_eq!(codex["exitCode"], 2);
    assert_eq!(codex["intercept"], "no");
    assert_eq!(codex["route"]["code"], "mitm-off");
    assert_eq!(
        codex["accounts"],
        serde_json::json!({"total": 1, "byGate": {"ok": 1}})
    );
    assert_eq!(codex["env"]["pointsAtTcr"], false);
    let verdict = codex["verdict"].as_str().expect("a verdict");
    assert!(
        verdict.starts_with("codex is not pooled: ") && !verdict.starts_with("verdict: "),
        "{verdict}"
    );

    // (c) `--codex --json` on the same proxy.
    let (code, stdout, stderr) = run_tcr_codex(
        home.path(),
        Some(&routed),
        &["doctor", "--codex", "--json", "--config", &config],
    );
    assert_eq!(code, Some(2), "stdout:\n{stdout}\nstderr:\n{stderr}");
    let document: serde_json::Value = serde_json::from_str(&stdout).expect("one JSON object");
    assert!(document.get("intercept").is_some(), "{document}");
    assert!(document.get("baseUrl").is_none(), "{document}");
    assert_eq!(document["exitCode"], 2);
}

/// A misspelled config key is named beside the Codex block, as it is in a Claude-only fleet:
/// after `requests10m:` and before `codexAccounts:`, and in `unknownKeys`. `--codex` names it
/// too, right before its verdict, since a misspelled `codexUpstream` is a Codex reader's
/// problem. Offline, where the upstream comes from the config, the misspelled key visibly sets
/// nothing.
#[test]
fn an_unknown_config_key_is_named_beside_the_codex_block() {
    const LINE: &str = "unknownKey: codexUpstrem (did you mean codexUpstream?)";
    let keys = serde_json::json!([{"key": "codexUpstrem", "nearest": "codexUpstream"}]);
    let home = tempfile::tempdir().expect("temp home");
    let write = |port: u16| {
        let dir = home.path().join(".config");
        std::fs::create_dir_all(&dir).expect("create the config directory");
        let path = dir.join("teamclaude.json");
        std::fs::write(
            &path,
            format!(
                "{{\"accounts\": [{ALICE}], \"proxy\": {{\"port\": {port}}}, \
                 \"codexUpstrem\": \"http://127.0.0.1:9\"}}"
            ),
        )
        .expect("write the config");
        path.display().to_string()
    };
    let port = serve_status(payload(
        vec![
            status_row("alice@example.com", false),
            status_row("bob@example.com/codex", true),
        ],
        Vec::new(),
        "none",
        &[],
    ));
    let config = write(port);
    let routed = format!("http://127.0.0.1:{port}");

    let (code, stdout, stderr) =
        run_tcr_codex(home.path(), Some(&routed), &["doctor", "--config", &config]);
    assert_eq!(code, Some(0), "stdout:\n{stdout}\nstderr:\n{stderr}");
    let lines: Vec<&str> = stdout.lines().collect();
    let at = |prefix: &str| {
        lines
            .iter()
            .position(|line| line.starts_with(prefix))
            .unwrap_or_else(|| panic!("no line starts {prefix:?}:\n{stdout}"))
    };
    assert_eq!(
        lines.iter().filter(|line| **line == LINE).count(),
        1,
        "{stdout}"
    );
    assert!(
        at("requests10m: ") < at(LINE) && at(LINE) < at("codexAccounts: "),
        "{stdout}"
    );
    // The upstream is the proxy's, which names none: the misspelled key cannot show here.
    assert!(
        lines.contains(&"codexUpstream: https://chatgpt.com (default)"),
        "{stdout}"
    );

    let (_, stdout, _) = run_tcr_codex(
        home.path(),
        Some(&routed),
        &["doctor", "--json", "--config", &config],
    );
    let document: serde_json::Value = serde_json::from_str(&stdout).expect("one JSON object");
    assert_eq!(document["unknownKeys"], keys);
    assert!(document["codex"].is_object(), "{document}");

    let (_, stdout, _) = run_tcr_codex(
        home.path(),
        Some(&routed),
        &["doctor", "--codex", "--config", &config],
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[lines.len() - 2], LINE, "{stdout}");
    assert!(lines[lines.len() - 1].starts_with("verdict: "), "{stdout}");
    let (_, stdout, _) = run_tcr_codex(
        home.path(),
        Some(&routed),
        &["doctor", "--codex", "--json", "--config", &config],
    );
    let document: serde_json::Value = serde_json::from_str(&stdout).expect("one JSON object");
    assert_eq!(document["unknownKeys"], keys);

    // Offline the upstream is the config's, and the misspelled key set nothing; honoured, it
    // would read `http://127.0.0.1:9 (set by codexUpstream)`.
    let offline = free_port_other_than(port);
    let config = write(offline);
    let (code, stdout, stderr) = run_tcr_codex(
        home.path(),
        Some(&format!("http://127.0.0.1:{offline}")),
        &["doctor", "--codex", "--config", &config],
    );
    assert_eq!(code, Some(3), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(
        stdout
            .lines()
            .any(|line| line == "codexUpstream: https://chatgpt.com (default)"),
        "{stdout}"
    );
}
