//! Tests for `tcr config set` and `tcr config show` in `cli/settings.rs`.

use super::*;
fn file_value(path: &Path, key: &str) -> serde_json::Value {
    let doc: serde_json::Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    doc.get(key).cloned().unwrap_or(serde_json::Value::Null)
}

/// With a proxy running, `config set` goes THROUGH it: the proxy writes the
/// key into its own config file, the running value stays the boot one, and
/// the proxy's next write of its own (a `tcr disable` through its route)
/// keeps the key.
///
/// The proxy serves a SECOND copy of the file (`served`), so a direct write
/// by the CLI would land in `cli_path` and leave `served` alone: the value
/// appearing in `served` and nowhere else is the proof the route did it.
#[tokio::test]
async fn config_set_goes_through_a_running_proxy_and_survives_its_next_write() {
    let (cli_path, listener) = config_on_a_scratch_port("config-set-live").await;
    let served = cli_path.with_extension("served.json");
    fs::copy(&cli_path, &served).unwrap();
    let cli_before = fs::read_to_string(&cli_path).unwrap();
    let manager =
        crate::test_support::live_proxy::serve(listener, load(&served), Some(served.clone()));

    set_config(&cli_path, "switchThreshold", "90%")
        .await
        .unwrap();

    assert_eq!(
        file_value(&served, "switchThreshold"),
        serde_json::json!(0.9)
    );
    assert_eq!(
        fs::read_to_string(&cli_path).unwrap(),
        cli_before,
        "the CLI wrote nothing itself"
    );
    assert_eq!(
        manager.running_settings().switch_threshold,
        0.95,
        "the running proxy keeps its boot value"
    );

    // The proxy's own next write: park bob through its route.
    set_enabled(&served, "bob@example.com", true).await.unwrap();
    assert_eq!(
        file_value(&served, "switchThreshold"),
        serde_json::json!(0.9),
        "the proxy's own write kept the saved setting"
    );
    assert_eq!(
        file_value(&served, "accounts")[1]["disabled"],
        serde_json::json!(true),
        "and that write did happen"
    );
    fs::remove_file(&cli_path).ok();
    fs::remove_file(&served).ok();
}

/// With nothing listening, `config set` writes the file itself.
#[tokio::test]
async fn config_set_without_a_proxy_writes_the_file() {
    let path = config_on_a_dead_port("config-set-offline").await;
    set_config(&path, "accountThrottle", "200 8").await.unwrap();
    set_config(&path, "pacing", "4").await.unwrap();
    assert_eq!(
        file_value(&path, "accountThrottle"),
        serde_json::json!({"minSpacingMs": 200, "burst": 8})
    );
    assert_eq!(
        file_value(&path, "pacing"),
        serde_json::json!({"maxInFlightPerAccount": 4})
    );
    assert_eq!(
        file_value(&path, "quotaProbeSeconds"),
        serde_json::json!(120)
    );
    fs::remove_file(&path).ok();
}

/// A refused value writes nothing, with or without a proxy, and a refusal
/// from the proxy's route (here: a proxy with no config file to save to)
/// does not fall back to writing the file.
#[tokio::test]
async fn each_refused_config_value_writes_nothing() {
    let offline = config_on_a_dead_port("config-set-refused").await;
    let (live, listener) = config_on_a_scratch_port("config-set-refused-live").await;
    // A proxy with no config file behind it: its route refuses every set.
    crate::test_support::live_proxy::serve(listener, load(&live), None);

    for path in [&offline, &live] {
        let before = fs::read_to_string(path).unwrap();
        for (key, value) in [
            ("switchThreshold", "0"),
            ("controlReserve", "0.6"),
            ("accountThrottle", "350 0"),
            ("pacing", "0"),
            ("http1Only", "maybe"),
            ("controlAccount", "alice@example.com"),
        ] {
            let err = set_config(path, key, value)
                .await
                .expect_err(&format!("{key} {value} is refused"));
            assert!(err.to_string().contains("not"), "{key} {value}: {err}");
            assert_eq!(before, fs::read_to_string(path).unwrap(), "{key} {value}");
        }
    }

    let before = fs::read_to_string(&live).unwrap();
    let err = set_config(&live, "switchThreshold", "0.9")
        .await
        .expect_err("the route refused it");
    assert!(err.to_string().contains("without a config file"), "{err}");
    assert_eq!(before, fs::read_to_string(&live).unwrap());
    fs::remove_file(&offline).ok();
    fs::remove_file(&live).ok();
}

/// `running` is absent with no proxy; with one, it is the proxy's boot
/// value even after a set, and the set key is the one waiting.
#[tokio::test]
async fn config_show_reports_running_only_with_a_proxy() {
    let offline = config_on_a_dead_port("config-show-offline").await;
    let shown = config_show_document(&offline).await.unwrap();
    assert!(shown.running.is_none());
    assert!(shown.waiting().is_empty());

    let (path, listener) = config_on_a_scratch_port("config-show-live").await;
    crate::test_support::live_proxy::serve(listener, load(&path), Some(path.clone()));

    let shown = config_show_document(&path).await.unwrap();
    assert_eq!(shown.waiting(), Vec::<&str>::new(), "nothing waits at boot");

    set_config(&path, "switchThreshold", "0.9").await.unwrap();
    set_config(&path, "http1Only", "on").await.unwrap();
    let shown = config_show_document(&path).await.unwrap();
    assert_eq!(shown.saved.switch_threshold, 0.9);
    let running = shown.running.as_ref().expect("a proxy answered");
    assert_eq!(running.switch_threshold, 0.95, "the boot value");
    assert!(!running.http1_only);
    assert_eq!(shown.waiting(), ["switchThreshold", "http1Only"]);
    fs::remove_file(&offline).ok();
    fs::remove_file(&path).ok();
}
