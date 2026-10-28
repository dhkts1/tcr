//! The read-only reset contract probe, fed synthetic stdin; no login or real endpoint is read.
use std::io::Write;
use std::process::{Command, Stdio};

fn probe(input: &[u8]) -> std::process::Output {
    let home = tempfile::tempdir().unwrap();
    for directory in ["xdg", "cache", "codex"] {
        std::fs::create_dir(home.path().join(directory)).unwrap();
    }
    let config = home.path().join("fleet.json");
    std::fs::write(&config, r#"{"accounts":[],"proxy":{"port":0}}"#).unwrap();
    let mut command = Command::new("/bin/bash");
    crate::tcr_env::isolate(&mut command, home.path());
    command
        .env("XDG_CONFIG_HOME", home.path().join("xdg"))
        .env("XDG_CACHE_HOME", home.path().join("cache"))
        .env("CODEX_HOME", home.path().join("codex"))
        .env("TMPDIR", home.path())
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/scripts/codex-contract-probe.sh"
        ))
        .args(["--resets", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

fn envelope() -> serde_json::Value {
    serde_json::json!({
        "usage": {"status":200, "body": {
            "rate_limit": null,
            "rate_limit_reset_credits":{"available_count":1},
            "fake-secret-key-sentinel":"fake-token-sentinel"
        }},
        "details": {"status":200, "body": {
            "available_count":1, "credits":[{
                "id":"fake-credit-sentinel", "reset_type":"codex_rate_limits", "status":"available",
                "granted_at":"2026-01-01T00:00:00Z", "expires_at":"2030-01-01T00:00:00Z",
                "title":"fake-title-sentinel", "description":"fake-description-sentinel"
            }]
        }}
    })
}

fn assert_private(output: &std::process::Output) {
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    for sentinel in [
        "fake-secret-key-sentinel",
        "fake-token-sentinel",
        "fake-credit-sentinel",
        "fake-title-sentinel",
        "fake-description-sentinel",
        "2030-01-01",
        "2026-01-01",
    ] {
        assert!(!text.contains(sentinel), "probe leaked a synthetic value");
    }
    assert!(
        output.stderr.is_empty(),
        "sanitized failures use stdout only"
    );
}

#[test]
fn reset_probe_prints_shapes_and_never_values_or_unknown_keys() {
    let output = probe(&serde_json::to_vec(&envelope()).unwrap());
    assert_private(&output);
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("details.credits[].id: string"));
    assert!(text.contains("usage.rate_limit_reset_credits.available_count: integer"));
    assert!(text.contains("reset contract: pass"));
    assert!(!text.contains("items"));
}

#[test]
fn reset_probe_fails_closed_without_disclosing_bad_payloads() {
    for input in [
        b"fake-token-sentinel".to_vec(),
        {
            let mut value = envelope();
            value["details"]["status"] = serde_json::json!(503);
            value["details"]["body"] = serde_json::json!("fake-token-sentinel");
            serde_json::to_vec(&value).unwrap()
        },
        {
            let mut value = envelope();
            value["details"]["body"]["credits"] = serde_json::json!([]);
            serde_json::to_vec(&value).unwrap()
        },
        {
            let mut value = envelope();
            value["details"]["body"]["credits"][0]["granted_at"] =
                serde_json::json!("fake-token-sentinel");
            serde_json::to_vec(&value).unwrap()
        },
    ] {
        let output = probe(&input);
        assert_private(&output);
        assert_eq!(output.status.code(), Some(1));
        assert!(
            String::from_utf8(output.stdout)
                .unwrap()
                .contains("reset contract: fail")
        );
    }
}

use axum::body::to_bytes;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tcr::test_support::stub_upstream as stub;

struct ResetProxy {
    home: tempfile::TempDir,
    config: PathBuf,
    consumes: Arc<AtomicUsize>,
    reads: Arc<AtomicUsize>,
    keys: Arc<Mutex<Vec<String>>>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}

impl ResetProxy {
    async fn new(code: &'static str, usage_ok: bool) -> Self {
        let consumes = Arc::new(AtomicUsize::new(0));
        let reads = Arc::new(AtomicUsize::new(0));
        let keys = Arc::new(Mutex::new(Vec::<String>::new()));
        let origin=stub::http(axum::Router::new().fallback({
            let consumes=Arc::clone(&consumes);let reads=Arc::clone(&reads);let keys=Arc::clone(&keys);
            move |req: Request| {
                let consumes=Arc::clone(&consumes);let reads=Arc::clone(&reads);let keys=Arc::clone(&keys);
                async move {
                    if req.uri().path()=="/backend-api/wham/rate-limit-reset-credits/consume" {
                        assert!(req.headers().get("authorization").is_some_and(|v|v=="Bearer fake-token-private-sentinel"));
                        assert!(req.headers().get("chatgpt-account-id").is_some_and(|v|v=="fake-account-private-sentinel"));
                        let body:serde_json::Value=serde_json::from_slice(&to_bytes(req.into_body(),8192).await.unwrap()).unwrap();
                        assert!(body.as_object().is_some_and(|o|o.len()==1&&o.contains_key("redeem_request_id")));
                        let key=body["redeem_request_id"].as_str().unwrap().to_owned();
                        let duplicate={let mut keys=keys.lock().unwrap();let duplicate=keys.contains(&key);keys.push(key);duplicate};
                        let n=consumes.fetch_add(1,Ordering::SeqCst);
                        if code=="lost"&&n==0 {return (StatusCode::OK,"fake-raw-private-sentinel").into_response();}
                        let outcome=if duplicate {"already_redeemed"} else if code=="lost" {"reset"} else {code};
                        return axum::Json(serde_json::json!({"code":outcome,"windows_reset":1,"ignored":"fake-credit-private-sentinel"})).into_response();
                    }
                    assert_eq!(req.uri().path(),"/backend-api/wham/usage");
                    reads.fetch_add(1,Ordering::SeqCst);
                    if !usage_ok {return (StatusCode::SERVICE_UNAVAILABLE,"fake-raw-private-sentinel").into_response();}
                    axum::Json(serde_json::json!({"rate_limit":{"allowed":true,"limit_reached":false},"rate_limit_reset_credits":{"available_count":0}})).into_response()
                }
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let home = tempfile::tempdir().unwrap();
        for name in ["xdg", "cache", "codex"] {
            std::fs::create_dir(home.path().join(name)).unwrap();
        }
        let config = home.path().join("fleet.json");
        let fleet = serde_json::json!({"accounts":[{"name":"alice@example.com","provider":"codex","type":"oauth","accessToken":"fake-token-private-sentinel",
            "accountUuid":"fake-account-private-sentinel","expiresAt":tcr::now_ms()+3_600_000}],
            "proxy":{"port":port,"apiKey":"fake-local-private-sentinel"},"upstream":origin.url(),"codexUpstream":origin.url()});
        std::fs::write(&config, serde_json::to_vec(&fleet).unwrap()).unwrap();
        let fleet: tcr::config::Config = serde_json::from_value(fleet).unwrap();
        let prober = tcr::probe::LiveUsageProber::new().with_codex_base(origin.url());
        let manager = tcr::manager::Manager::new(
            fleet,
            Arc::new(tcr::oauth::NoRefresh),
            Arc::new(prober),
            Arc::new(tcr::warmer::LiveWarmer::new()),
            None,
        );
        let app = tcr::proxy::app(manager).layer(axum::Extension(tcr::proxy::ClientAddr(
            "127.0.0.1:1234".parse().unwrap(),
        )));
        let (shutdown, done) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    done.await.ok();
                })
                .await
        });
        Self {
            home,
            config,
            consumes,
            reads,
            keys,
            shutdown: Some(shutdown),
            task: Some(task),
        }
    }

    async fn run(&self, input: &'static [u8], explicit_key: bool) -> std::process::Output {
        let mut command = reset_command(self.home.path(), &self.config);
        if explicit_key {
            command.args(["--idempotency-key", "fake-attempt-private-sentinel"]);
        }
        tokio::task::spawn_blocking(move || {
            let mut child = command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(input).unwrap();
            child.wait_with_output().unwrap()
        })
        .await
        .unwrap()
    }

    async fn finish(mut self) {
        self.shutdown.take().unwrap().send(()).unwrap();
        self.task.take().unwrap().await.unwrap().unwrap();
    }
}

impl Drop for ResetProxy {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

fn reset_command(home: &Path, config: &Path) -> Command {
    let mut command = crate::tcr_env::tcr(home);
    command
        .env("XDG_CONFIG_HOME", home.join("xdg"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env("CODEX_HOME", home.join("codex"))
        .env("TMPDIR", std::env::temp_dir())
        .args(["codex", "reset", "alice@example.com", "--config"])
        .arg(config);
    command
}

fn reset_output(output: &std::process::Output) -> (String, String) {
    let out = String::from_utf8(output.stdout.clone()).unwrap();
    let err = String::from_utf8(output.stderr.clone()).unwrap();
    for private in [
        "fake-token-private-sentinel",
        "fake-account-private-sentinel",
        "fake-credit-private-sentinel",
        "fake-local-private-sentinel",
        "fake-attempt-private-sentinel",
        "fake-raw-private-sentinel",
    ] {
        assert!(
            !out.contains(private) && !err.contains(private),
            "reset output must never expose backend or local secrets"
        );
    }
    (out, err)
}

#[tokio::test]
async fn reset_cli_all_outcomes_have_the_approved_text_and_exit() {
    for (code, text, exit) in [
        ("reset", "Applied.", 0),
        ("nothing_to_reset", "Nothing to reset.", 0),
        ("no_credit", "No reset credit available.", 6),
        ("already_redeemed", "Already redeemed.", 0),
    ] {
        let proxy = ResetProxy::new(code, true).await;
        let output = proxy.run(b"y\n", true).await;
        if code == "no_credit" {
            let no_credit = &output;
            assert_eq!(no_credit.status.code(), Some(6));
        }
        assert_eq!(output.status.code(), Some(exit));
        let (out, err) = reset_output(&output);
        assert_eq!(
            out,
            format!("Redeem one reset for alice@example.com? [y/N]\n{text}\n")
        );
        assert!(err.is_empty());
        assert_eq!(proxy.consumes.load(Ordering::SeqCst), 1);
        assert_eq!(proxy.reads.load(Ordering::SeqCst), 1);
        proxy.finish().await;
    }
}

#[tokio::test]
async fn reset_cli_cancel_default_and_eof_send_nothing() {
    let proxy = ResetProxy::new("reset", true).await;
    for input in [b"".as_slice(), b"\n", b"n\n"] {
        let output = proxy.run(input, false).await;
        assert_eq!(output.status.code(), Some(0));
        let (out, err) = reset_output(&output);
        assert_eq!(
            out,
            "Redeem one reset for alice@example.com? [y/N]\nCancelled.\n"
        );
        assert!(err.is_empty());
    }
    assert_eq!(proxy.consumes.load(Ordering::SeqCst), 0);
    assert_eq!(proxy.reads.load(Ordering::SeqCst), 0);
    proxy.finish().await;
}

#[tokio::test]
async fn reset_cli_retry_retains_key_and_new_confirmation_mints_a_new_attempt() {
    let proxy = ResetProxy::new("lost", true).await;
    let output = proxy.run(b"y\ny\n", false).await;
    assert_eq!(output.status.code(), Some(0));
    let (out, err) = reset_output(&output);
    assert_eq!(
        out,
        "Redeem one reset for alice@example.com? [y/N]\nRetry this reset attempt? [y/N]\nAlready redeemed.\n"
    );
    assert_eq!(err, "Reset outcome unknown.\n");
    {
        let keys = proxy.keys.lock().unwrap();
        assert!(
            keys.len() == 2 && keys[0] == keys[1],
            "explicit retry must retain the attempt key"
        );
    }
    let next = proxy.run(b"y\n", false).await;
    assert_eq!(next.status.code(), Some(0));
    reset_output(&next);
    {
        let keys = proxy.keys.lock().unwrap();
        assert!(
            keys.len() == 3 && keys[0] != keys[2],
            "new consent must create a new attempt"
        );
    }
    proxy.finish().await;
}

#[tokio::test]
async fn reset_cli_known_outcome_with_failed_usage_exits_five_and_keeps_outcome() {
    let proxy = ResetProxy::new("reset", false).await;
    let output = proxy.run(b"y\n", true).await;
    assert_eq!(output.status.code(), Some(5));
    let (out, err) = reset_output(&output);
    assert_eq!(
        out,
        "Redeem one reset for alice@example.com? [y/N]\nApplied.\n"
    );
    assert_eq!(err, "Usage refresh failed.\n");
    proxy.finish().await;
}

#[tokio::test]
async fn reset_cli_unknown_outcome_without_retry_exits_four() {
    let proxy = ResetProxy::new("future_code", true).await;
    let output = proxy.run(b"y\nn\n", true).await;
    assert_eq!(output.status.code(), Some(4));
    let (out, err) = reset_output(&output);
    assert_eq!(
        out,
        "Redeem one reset for alice@example.com? [y/N]\nRetry this reset attempt? [y/N]\n"
    );
    assert_eq!(err, "Reset outcome unknown.\n");
    assert_eq!(proxy.consumes.load(Ordering::SeqCst), 1);
    proxy.finish().await;
}

#[tokio::test]
async fn reset_cli_missing_proxy_has_no_offline_fallback() {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("fleet.json");
    std::fs::write(&config, r#"{"accounts":[],"proxy":{"port":0}}"#).unwrap();
    let mut command = reset_command(home.path(), &config);
    let output = tokio::task::spawn_blocking(move || {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(b"y\n").unwrap();
        child.wait_with_output().unwrap()
    })
    .await
    .unwrap();
    assert_eq!(output.status.code(), Some(3));
    let (_, err) = reset_output(&output);
    assert_eq!(err, "No running proxy.\n");
}

#[tokio::test]
async fn reset_cli_requires_stamp_refuses_old_proxy_and_never_redirects() {
    for (status, stamp, delay) in [
        (StatusCode::OK, false, 0),
        (StatusCode::NOT_FOUND, false, 0),
        (StatusCode::BAD_REQUEST, true, 0),
        (StatusCode::TEMPORARY_REDIRECT, true, 0),
        (StatusCode::OK, true, 6),
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let calls = Arc::new(AtomicUsize::new(0));
        let called = Arc::clone(&calls);
        let app=axum::Router::new().fallback(move |req:Request| {
            let calls=Arc::clone(&called);
            async move {
                calls.fetch_add(1,Ordering::SeqCst);
                assert_eq!(req.uri().path(),"/_tcr/codex/reset","the CLI must never follow a reset redirect");
                assert!(req.headers().get("x-api-key").is_some_and(|v|v=="fake-local-private-sentinel"));
                tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
                let mut response=(status,[("location",format!("http://127.0.0.1:{port}/redirect-target"))],
                    r#"{"outcome":"applied","usageRefreshed":true,"error":{"message":"fake-raw-private-sentinel"}}"#).into_response();
                // The valid delayed answer omits the deliberately hostile error/extra field.
                if delay>0 {response=(status,r#"{"outcome":"applied","usageRefreshed":true}"#).into_response();}
                if stamp {response.headers_mut().insert("x-tcr-endpoint","codex-reset".parse().unwrap());}
                response
            }
        });
        let (shutdown, done) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    done.await.ok();
                })
                .await
        });
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join("fleet.json");
        std::fs::write(&config,serde_json::to_vec(&serde_json::json!({"accounts":[],"proxy":{"port":port,"apiKey":"fake-local-private-sentinel"}})).unwrap()).unwrap();
        let mut command = reset_command(home.path(), &config);
        let output = tokio::task::spawn_blocking(move || {
            let mut child = command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(b"y\nn\n").unwrap();
            child.wait_with_output().unwrap()
        })
        .await
        .unwrap();
        shutdown.send(()).unwrap();
        task.await.unwrap().unwrap();
        let (out, err) = reset_output(&output);
        assert_eq!(output.status.code(), Some(if delay > 0 { 0 } else { 4 }));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the CLI must neither follow redirects nor retry automatically"
        );
        if delay > 0 {
            assert!(out.ends_with("Applied.\n"));
            assert!(err.is_empty());
        } else {
            assert!(err.starts_with("Reset failed.") || err.starts_with("Reset outcome unknown."));
        }
    }
}

#[tokio::test]
async fn reset_cli_bad_config_and_confirmation_fail_safely_without_request() {
    let proxy = ResetProxy::new("reset", true).await;
    let output = proxy.run(b"\xff", true).await;
    assert_eq!(output.status.code(), Some(4));
    let (_, err) = reset_output(&output);
    assert_eq!(err, "Reset failed. Could not read confirmation.\n");
    std::fs::write(&proxy.config, "fake-raw-private-sentinel").unwrap();
    let output = proxy.run(b"y\n", true).await;
    assert_eq!(output.status.code(), Some(4));
    let (_, err) = reset_output(&output);
    assert_eq!(
        err,
        "Reset failed. Could not read the proxy configuration.\n"
    );
    assert_eq!(proxy.consumes.load(Ordering::SeqCst), 0);
    proxy.finish().await;
    let proxy = ResetProxy::new("future_code", true).await;
    let output = proxy.run(b"y\n\xff", true).await;
    assert_eq!(output.status.code(), Some(4));
    reset_output(&output);
    assert_eq!(proxy.consumes.load(Ordering::SeqCst), 1);
    proxy.finish().await;
}
