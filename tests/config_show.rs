//! `tcr config show [--json]` end to end against the BUILT BINARY.
//!
//! TcrBar's Settings window runs exactly `config show --json` (plus
//! `--config` here) and decodes the object, so these tests pin the argv and
//! the keys it reads. Every run uses a scratch `HOME` and a config file inside
//! it; no test reads the operator's config or starts a server.

use std::path::PathBuf;
use std::process::Command;

struct Scratch {
    _dir: tempfile::TempDir,
    home: PathBuf,
    config: PathBuf,
}

fn scratch(config_json: &str) -> Scratch {
    let dir = tempfile::tempdir().expect("scratch HOME");
    let home = dir.path().to_path_buf();
    let config = home.join("tcr.json");
    std::fs::write(&config, config_json).expect("write the scratch config");
    Scratch {
        _dir: dir,
        home,
        config,
    }
}

fn tcr(scratch: &Scratch, args: &[&str]) -> (i32, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_tcr"))
        .args(args)
        .arg("--config")
        .arg(&scratch.config)
        .env("HOME", &scratch.home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_CACHE_HOME")
        .current_dir(&scratch.home)
        .output()
        .expect("run the built tcr");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn json_reports_the_effective_values_of_a_set_file() {
    let s = scratch(
        r#"{
            "accounts": [],
            "switchThreshold": 0.9,
            "controlAccount": "alice@example.com",
            "fleetThrottle": {},
            "pacing": {"maxInFlightPerAccount": 4},
            "sessionAffinity": false,
            "usageRetentionDays": 30
        }"#,
    );
    let (code, stdout, stderr) = tcr(&s, &["config", "show", "--json"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("one JSON object");
    assert_eq!(value["switchThreshold"], 0.9);
    assert_eq!(
        value["fableWeeklyThreshold"], 0.9,
        "follows switchThreshold"
    );
    assert_eq!(value["controlReserve"], 0.05);
    assert_eq!(value["resetUrgencyTierHours"], 24);
    assert_eq!(value["sessionAffinity"], false);
    assert_eq!(value["controlAccount"], "alice@example.com");
    assert_eq!(value["controlPooled"], false);
    assert_eq!(
        value["accountThrottle"],
        serde_json::json!({"minSpacingMs": 350, "burst": 24})
    );
    assert_eq!(value["fleetThrottle"], serde_json::Value::Null);
    assert_eq!(
        value["pacing"],
        serde_json::json!({"maxInFlightPerAccount": 4, "minSpacingMs": null})
    );
    assert_eq!(value["usageRetentionDays"], 30);
    assert_eq!(value["http1Only"], false);
    assert_eq!(
        value["configPath"],
        s.config.to_string_lossy().as_ref(),
        "names the file it read"
    );
}

#[test]
fn the_table_names_each_setting_in_words() {
    let s = scratch(r#"{"accounts": []}"#);
    let (code, stdout, stderr) = tcr(&s, &["config", "show"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("switchThreshold"), "{stdout}");
    assert!(stdout.contains("1 per 350 ms, bursts of 24"), "{stdout}");
    assert!(stdout.contains("90 days"), "{stdout}");
}

#[test]
fn a_file_that_does_not_parse_is_an_error_not_defaults() {
    let s = scratch("{ not json");
    let (code, stdout, _stderr) = tcr(&s, &["config", "show", "--json"]);
    assert_ne!(code, 0, "a broken file must not read as the defaults");
    assert!(stdout.is_empty(), "nothing on stdout: {stdout}");
}
