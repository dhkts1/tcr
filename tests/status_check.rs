//! `tcr status --check` end to end, against a scratch HOME and a fake status
//! endpoint.
//!
//! The flag's whole contract is the process exit code, so this tests the BUILT
//! BINARY: the rule that decides "failed" has unit tests beside it in
//! `src/cli.rs`, but whether `main` actually hands that decision to the process
//! (and leaves the default exit alone) exists only here.
//!
//! The payload the fake endpoint serves is not hand-written. It is the real
//! producer's output: an offline snapshot over a scripted [`UsageProber`] (a 401
//! for the failing fleet, a fixed bar for the healthy one), projected onto the
//! wire by [`StatusPayload::from_snapshot`], the function the server itself
//! calls. So a row reads `probeStatus: "error"` here for the same reason it
//! would on a live proxy whose probe was rejected.
//!
//! Isolation follows `tests/doctor.rs`: a temp HOME, an explicit config, and a
//! kernel-assigned port, so the live proxy is never the thing a test talks to.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;

use std::sync::Arc;

use tcr::cli::snapshot_offline;
use tcr::oauth::NoRefresh;
use tcr::probe::{ProbeCredential, ProbeError, ProbeFuture, Usage, UsageBucket, UsageProber};
use tcr::status::StatusPayload;

/// The exit code `--check` reports a failed probe with
/// (`tcr::cli::StatusCheck::exit_code`). Written out as a literal on
/// purpose: it is a contract scripts read, so changing it must fail here.
const PROBE_FAILED_EXIT: i32 = 4;

/// Rejects every token the way the usage endpoint rejects a dead one.
struct RejectingProber;
impl UsageProber for RejectingProber {
    fn probe(&self, _credential: ProbeCredential) -> ProbeFuture {
        Box::pin(async {
            Err(ProbeError {
                status: Some(401),
                message: "HTTP 401: token rejected".to_string(),
                retry_after_secs: None,
            })
        })
    }
}

/// Answers every probe with a fixed bar and no reset instant, so the rendered
/// text carries no countdown that could change between two runs.
struct HealthyProber;
impl UsageProber for HealthyProber {
    fn probe(&self, _credential: ProbeCredential) -> ProbeFuture {
        Box::pin(async {
            Ok(Usage {
                five_hour: Some(UsageBucket {
                    utilization: Some(0.2),
                    reset_at_ms: None,
                }),
                seven_day: None,
                seven_day_oi: None,
                extra_usage_usd: None,
                codex: None,
            })
        })
    }
}

/// One OAuth account whose access token is valid until 2100, so the probe runs
/// without a refresh and its outcome is the prober's alone.
const FLEET: &str = r#"{
  "accounts": [
    { "name": "alice@example.com", "type": "oauth", "accessToken": "at-alice",
      "refreshToken": "rt-alice", "expiresAt": 4102444800000, "priority": 0 }
  ]
}"#;

/// The wire body a live proxy would serve for [`FLEET`] probed by `prober`.
async fn payload_from(dir: &Path, prober: Arc<dyn UsageProber>) -> String {
    let path = dir.join("fleet.json");
    std::fs::write(&path, FLEET).expect("write the fleet config");
    let config = tcr::config::load(&path).expect("load the fleet config");
    let snapshot = snapshot_offline(config, Arc::new(NoRefresh), prober, true).await;
    let thresholds = vec![1.0; snapshot.accounts.len()];
    let payload = StatusPayload::from_snapshot(
        &snapshot,
        &thresholds,
        false,
        None,
        std::collections::BTreeMap::new(),
    );
    serde_json::to_string(&payload).expect("serialize the status payload")
}

/// A `/_tcr/status` endpoint on a kernel-assigned port that answers every
/// connection with `body`. Returns the port. The accept loop outlives the
/// test's use of it on purpose, for the reason `tests/doctor.rs` gives.
fn fake_status_endpoint(body: String) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a scratch port");
    let port = listener.local_addr().expect("local addr").port();
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

fn serve_one(stream: TcpStream, body: &str) {
    serve_status(stream, "200 OK", body);
}

/// Read one request off `stream` and answer it `status` with the JSON `body`.
fn serve_status(mut stream: TcpStream, status: &str, body: &str) {
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
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _written = stream.write_all(response.as_bytes());
    let _flushed = stream.flush();
}

/// A config with no accounts of its own, pointed at `port`. With a proxy
/// answering there, `tcr status` reads the fleet from it and never probes.
fn write_config(home: &Path, port: u16) -> std::path::PathBuf {
    let dir = home.join(".config");
    std::fs::create_dir_all(&dir).expect("create the config directory");
    let path = dir.join("tcr.json");
    std::fs::write(
        &path,
        format!("{{\n  \"accounts\": [],\n  \"proxy\": {{ \"port\": {port} }}\n}}\n"),
    )
    .expect("write the config");
    path
}

/// Run `tcr status <extra…>` with a scratch HOME and working directory, routed
/// at the fake proxy so no route verdict line appears.
fn run_status(home: &Path, config: &Path, port: u16, extra: &[&str]) -> (Option<i32>, String) {
    let work = home.join("work");
    std::fs::create_dir_all(&work).expect("create the scratch working directory");
    let config = config.display().to_string();
    let mut args = vec!["status", "--config", config.as_str()];
    args.extend_from_slice(extra);
    // The route line reads the shell's startup files for `tcr setup shell`'s block; the
    // helper keeps that read inside the scratch HOME.
    let out = crate::tcr_env::tcr(home)
        .args(&args)
        .env("ANTHROPIC_BASE_URL", format!("http://127.0.0.1:{port}"))
        .current_dir(&work)
        .output()
        .expect("run tcr");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    (
        out.status.code(),
        format!("{stdout}\n--- stderr ---\n{stderr}"),
    )
}

fn stdout_of(combined: &str) -> &str {
    combined
        .split("\n--- stderr ---\n")
        .next()
        .unwrap_or_default()
}

#[tokio::test]
async fn check_exits_nonzero_when_a_probe_failed_and_the_default_exit_stays_zero() {
    let home = tempfile::tempdir().expect("temp home");
    let body = payload_from(home.path(), Arc::new(RejectingProber)).await;
    assert!(
        body.contains("\"probeStatus\":\"error\""),
        "the fixture must carry a failed probe, or this test proves nothing: {body}"
    );
    let port = fake_status_endpoint(body);
    let config = write_config(home.path(), port);

    for json in [false, true] {
        let form: &[&str] = if json { &["--json"] } else { &[] };
        let with_check: Vec<&str> = form.iter().copied().chain(["--check"]).collect();

        // The default exit is TcrBar's contract: `StatusPoller.classify` reads
        // any non-zero exit as a failed poll and blanks the panel.
        let (plain_code, plain) = run_status(home.path(), &config, port, form);
        assert_eq!(
            plain_code,
            Some(0),
            "json={json}, without --check:\n{plain}"
        );
        assert!(
            stdout_of(&plain).contains("source") && stdout_of(&plain).contains("live"),
            "the read must come from the fake proxy, not an offline probe:\n{plain}"
        );

        let (checked_code, checked) = run_status(home.path(), &config, port, &with_check);
        assert_eq!(
            checked_code,
            Some(PROBE_FAILED_EXIT),
            "json={json}, with --check over a failed probe:\n{checked}"
        );
        assert_eq!(
            stdout_of(&checked),
            stdout_of(&plain),
            "json={json}: --check must change the exit code only, never stdout"
        );
    }
}

#[tokio::test]
async fn check_exits_zero_when_every_probe_is_ok() {
    let home = tempfile::tempdir().expect("temp home");
    let body = payload_from(home.path(), Arc::new(HealthyProber)).await;
    assert!(
        body.contains("\"probeStatus\":\"ok\""),
        "the fixture must carry a healthy probe: {body}"
    );
    let port = fake_status_endpoint(body);
    let config = write_config(home.path(), port);

    for form in [&["--check"][..], &["--json", "--check"][..]] {
        let (code, out) = run_status(home.path(), &config, port, form);
        assert_eq!(code, Some(0), "{form:?} over a healthy fleet:\n{out}");
    }
}

// `tcr routing --json`: its whole contract is stdout and the exit code a launcher reads.

/// The exits `tcr routing --json` answers with, written out as literals on purpose: a
/// launcher reads them, so changing one must fail here.
const ROUTING_SERVED_EXIT: i32 = 0;
const ROUTING_NO_PROXY_EXIT: i32 = 3;
const ROUTING_UNREADABLE_EXIT: i32 = 4;

/// An obviously fake proxy key for the in-process proxy, and a second one it refuses.
const ROUTING_KEY: &str = "4444444444444444444444444444444444444444444444444444444444444444";
const WRONG_KEY: &str = "5555555555555555555555555555555555555555555555555555555555555555";

/// A config with no accounts pointed at `port`, carrying `key` as its proxy api-key.
fn write_routing_config(home: &Path, port: u16, key: &str) -> std::path::PathBuf {
    let dir = home.join(".config");
    std::fs::create_dir_all(&dir).expect("create the config directory");
    let path = dir.join("tcr.json");
    std::fs::write(
        &path,
        format!(
            "{{\n  \"accounts\": [],\n  \"proxy\": {{ \"port\": {port}, \"apiKey\": \"{key}\" }}\n}}\n"
        ),
    )
    .expect("write the config");
    path
}

/// A real proxy in this process, keyed with [`ROUTING_KEY`], on a kernel-assigned port: one
/// invented account, upstreams on dead ports, nothing sent anywhere. Returns the port.
async fn routing_proxy(home: &Path) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a scratch port");
    let port = listener.local_addr().expect("local addr").port();
    let path = home.join("proxy.json");
    std::fs::write(
        &path,
        format!(
            r#"{{ "proxy": {{ "port": {port}, "apiKey": "{ROUTING_KEY}" }}, "upstream": "http://127.0.0.1:0", "codexUpstream": "http://127.0.0.1:0", "accounts": [{{ "name": "alice@example.com", "type": "oauth", "accessToken": "at-alice", "expiresAt": 4102444800000, "priority": 0 }}] }}"#
        ),
    )
    .expect("write the proxy config");
    let config = tcr::config::load(&path).expect("load the proxy config");
    let manager = tcr::manager::Manager::with_live_refresher(config, None);
    tokio::spawn(async move { tcr::mitm::serve(listener, manager, None).await });
    port
}

/// A port that answers every request `status` with `body`.
fn fake_answer(status: &'static str, body: &'static str) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a scratch port");
    let port = listener.local_addr().expect("local addr").port();
    std::thread::spawn(move || {
        while let Ok((stream, _)) = listener.accept() {
            serve_status(stream, status, body);
        }
    });
    port
}

/// A port that accepts every connection and never answers.
fn silent_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a scratch port");
    let port = listener.local_addr().expect("local addr").port();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept() {
            held.push(stream);
        }
    });
    port
}

/// A port nothing listens on: bound by the kernel, then released.
fn closed_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind a scratch port")
        .local_addr()
        .expect("local addr")
        .port()
}

/// Run `tcr routing --json` against `config`, off the async runtime, because the proxy it
/// may talk to is served by this same runtime. Returns the exit, stdout and stderr.
async fn run_routing(home: &Path, config: &Path) -> (Option<i32>, String, String) {
    let home = home.to_path_buf();
    let config = config.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let out = crate::tcr_env::tcr(&home)
            .args(["routing", "--json", "--config"])
            .arg(&config)
            .current_dir(&home)
            .output()
            .expect("run tcr");
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    })
    .await
    .expect("join the tcr run")
}

/// **Served: exit 0, and stdout is the running proxy's own schema-1 document.**
#[tokio::test]
async fn routing_prints_the_served_document_and_exits_0() {
    let home = tempfile::tempdir().expect("temp home");
    let port = routing_proxy(home.path()).await;
    let config = write_routing_config(home.path(), port, ROUTING_KEY);
    let (code, stdout, stderr) = run_routing(home.path(), &config).await;
    assert_eq!(
        code,
        Some(ROUTING_SERVED_EXIT),
        "{stdout}\n--- stderr ---\n{stderr}"
    );
    let document: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("stdout is the JSON document");
    assert_eq!(document["schema"], 1);
    assert_eq!(document["providers"][0]["provider"], "anthropic");
    assert_eq!(document["providers"][1]["provider"], "codex");
    assert!(document["forecast"].is_null());
}

/// **Nothing listening: exit 3, nothing on stdout, and no document made up offline.**
#[tokio::test]
async fn routing_exits_3_when_no_proxy_listens() {
    let home = tempfile::tempdir().expect("temp home");
    let config = write_routing_config(home.path(), closed_port(), ROUTING_KEY);
    let (code, stdout, stderr) = run_routing(home.path(), &config).await;
    assert_eq!(
        code,
        Some(ROUTING_NO_PROXY_EXIT),
        "{stdout}\n--- stderr ---\n{stderr}"
    );
    assert!(stdout.is_empty(), "no offline document: {stdout}");
    assert!(!stderr.trim().is_empty(), "the reason goes to stderr");
}

/// **A proxy answered, but not with a readable schema-1 document: exit 4**, whether it
/// refused the key, answered an HTTP error, never answered in time, or answered something
/// that is not schema 1.
#[tokio::test]
async fn routing_exits_4_on_auth_http_timeout_unreadable_or_unknown_schema() {
    let home = tempfile::tempdir().expect("temp home");
    let proxy = routing_proxy(home.path()).await;
    let cases = [
        ("a refused key", proxy, WRONG_KEY),
        (
            "an HTTP error",
            fake_answer("500 Internal Server Error", "{}"),
            ROUTING_KEY,
        ),
        ("no answer in time", silent_port(), ROUTING_KEY),
        (
            "an unreadable document",
            fake_answer("200 OK", "not json"),
            ROUTING_KEY,
        ),
        (
            "an unknown schema",
            fake_answer("200 OK", r#"{"schema":2}"#),
            ROUTING_KEY,
        ),
    ];
    for (name, port, key) in cases {
        let config = write_routing_config(home.path(), port, key);
        let (code, stdout, stderr) = run_routing(home.path(), &config).await;
        assert_eq!(
            code,
            Some(ROUTING_UNREADABLE_EXIT),
            "{name}:\n{stdout}\n--- stderr ---\n{stderr}"
        );
        assert!(stdout.is_empty(), "{name}: nothing on stdout: {stdout}");
        assert!(
            !stderr.trim().is_empty(),
            "{name}: the reason goes to stderr"
        );
    }
}
