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
