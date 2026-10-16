//! `tcr config show [--json]` end to end against the BUILT BINARY.
//!
//! TcrBar's Settings window runs exactly `config show --json` and `config set
//! <key> <value>...` (plus `--config` here) and decodes the object, so these
//! tests pin the argvs and the keys it reads. Every run uses a scratch `HOME`
//! and a config file inside it; no test reads the operator's config or starts
//! a server.
//!
//! Every config here names a port nothing listens on ([`dead_port`]).
//! `config show` and `config set` ask the proxy on the config's port, and a
//! config with no port means 3456, which is the live proxy on a working Mac.

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

/// A loopback port that was free a moment ago: bound, read, and released.
fn dead_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a scratch port");
    listener.local_addr().expect("its address").port()
}

/// `{"proxy": {"port": <dead>}, "accounts": [], <rest>}`.
fn config_on_a_dead_port(rest: &str) -> String {
    let rest = rest.trim();
    let separator = if rest.is_empty() { "" } else { ", " };
    format!(
        r#"{{"proxy": {{"port": {}}}, "accounts": []{separator}{rest}}}"#,
        dead_port()
    )
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
    let s = scratch(&config_on_a_dead_port(
        r#"
            "switchThreshold": 0.9,
            "controlAccount": "alice@example.com",
            "fleetThrottle": {},
            "pacing": {"maxInFlightPerAccount": 4},
            "sessionAffinity": false,
            "usageRetentionDays": 30
        "#,
    ));
    let (code, stdout, stderr) = tcr(&s, &["config", "show", "--json"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("one JSON object");
    assert!(
        value.get("running").is_none(),
        "no proxy answered, so no running object: {value}"
    );
    assert_eq!(value["fableWeeklyThresholdFollows"], true);
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
    let s = scratch(&config_on_a_dead_port(""));
    let (code, stdout, stderr) = tcr(&s, &["config", "show"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("switchThreshold"), "{stdout}");
    assert!(stdout.contains("1 per 350 ms, bursts of 24"), "{stdout}");
    assert!(stdout.contains("90 days"), "{stdout}");
    assert!(stdout.contains("nothing is waiting"), "{stdout}");
}

/// The argv TcrBar runs for a two-number setting and a one-number one, each
/// followed by `--config`: the value list must stop at the flag, and the file
/// then holds what `config show` reads back.
#[test]
fn set_writes_the_file_and_show_reads_it_back() {
    let s = scratch(&config_on_a_dead_port(r#""someoneElses": true"#));
    let (code, stdout, stderr) = tcr(&s, &["config", "set", "accountThrottle", "200", "8"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("Saved accountThrottle"), "{stdout}");
    let (code, _, stderr) = tcr(&s, &["config", "set", "pacing", "4"]);
    assert_eq!(code, 0, "stderr: {stderr}");

    let (code, stdout, stderr) = tcr(&s, &["config", "show", "--json"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("one JSON object");
    assert_eq!(
        value["accountThrottle"],
        serde_json::json!({"minSpacingMs": 200, "burst": 8})
    );
    assert_eq!(value["pacing"]["maxInFlightPerAccount"], 4);
    let raw: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&s.config).expect("read")).expect("json");
    assert_eq!(raw["someoneElses"], true, "a key tcr does not know stays");
}

/// A refused value exits non-zero with its sentence and leaves the file
/// byte-identical, `-1` included (it must reach the parser, not clap).
#[test]
fn a_refused_value_changes_nothing() {
    let s = scratch(&config_on_a_dead_port(""));
    let before = std::fs::read_to_string(&s.config).expect("read");
    for args in [
        ["config", "set", "pacing", "-1"],
        ["config", "set", "controlReserve", "0.6"],
        ["config", "set", "switchThreshold", "0"],
    ] {
        let (code, _, stderr) = tcr(&s, &args);
        assert_ne!(code, 0, "{args:?} is refused");
        assert!(
            stderr.contains("Nothing was changed."),
            "{args:?}: {stderr}"
        );
        assert_eq!(
            before,
            std::fs::read_to_string(&s.config).expect("read"),
            "{args:?}"
        );
    }
}

#[test]
fn a_file_that_does_not_parse_is_an_error_not_defaults() {
    let s = scratch("{ not json");
    let (code, stdout, _stderr) = tcr(&s, &["config", "show", "--json"]);
    assert_ne!(code, 0, "a broken file must not read as the defaults");
    assert!(stdout.is_empty(), "nothing on stdout: {stdout}");
}
