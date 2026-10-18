//! The `fanControl` contract: every sample in the committed fixture
//! (`tests/fixtures/fan-control-contract.json`) decodes into [`FanControlRow`] and
//! re-encodes to the same JSON value. TcrBar's popover reads the same file, so the two
//! sides check themselves against one copy.

use std::path::Path;

use tcr_status_wire::{FanControlRow, FanDesiredRow, FanReasonRow, FanRow, FanStateRow, HeatRow};

fn samples() -> serde_json::Map<String, serde_json::Value> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/fan-control-contract.json");
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("the fan control fixture is unreadable: {e}"));
    match serde_json::from_str(&text) {
        Ok(serde_json::Value::Object(samples)) => samples,
        other => panic!("the fan control fixture is an object of samples, not {other:?}"),
    }
}

#[test]
fn every_fan_control_sample_round_trips() {
    let samples = samples();
    assert!(samples.len() >= 3, "{samples:?}");
    for (name, sample) in samples {
        let row: FanControlRow = serde_json::from_value(sample.clone())
            .unwrap_or_else(|e| panic!("sample {name} does not decode: {e}"));
        assert_eq!(
            serde_json::to_value(&row).unwrap(),
            sample,
            "sample {name} does not re-encode to itself"
        );
    }
}

#[test]
fn the_fan_control_samples_say_what_their_names_say() {
    let samples = samples();
    let row =
        |name: &str| -> FanControlRow { serde_json::from_value(samples[name].clone()).unwrap() };
    let off = row("off");
    assert_eq!(off.desired, FanDesiredRow::Auto);
    assert_eq!((off.state, off.reason), (FanStateRow::Off, None));
    let fixed = row("holdingFixed5000");
    assert_eq!(fixed.desired, FanDesiredRow::Fixed { rpm: 5000 });
    assert_eq!(fixed.state, FanStateRow::Holding);
    let follow = row("holdingFollow");
    assert_eq!(
        follow.desired,
        FanDesiredRow::FollowCpu {
            quiet_below_c: 60,
            full_at_c: 90
        }
    );
    let foreign = row("foreignMacsFanControl");
    assert_eq!(
        (foreign.state, foreign.reason),
        (FanStateRow::Foreign, Some(FanReasonRow::MacsFanControl))
    );
}

/// An unknown state from a newer server is a decode error for the whole object, never a
/// guess: the main crate's status field keeps that error beside the rest of the payload.
#[test]
fn an_unknown_state_does_not_decode() {
    let mut sample = samples()["off"].clone();
    sample["state"] = serde_json::json!("hibernating");
    assert!(serde_json::from_value::<FanControlRow>(sample).is_err());
}

/// Stage 1's heat gains two optional keys; an old reading without them still decodes, and
/// neither is written when absent.
#[test]
fn heat_target_and_ftst_are_optional() {
    let old: HeatRow = serde_json::from_value(serde_json::json!({
        "cpuCelsius": 50.0,
        "fans": [{"index": 0, "actualRpm": 1400.0, "minRpm": 1350.0, "maxRpm": 5777.0, "manual": false}]
    }))
    .unwrap();
    assert_eq!(old.ftst, None);
    assert_eq!(old.fans[0].target_rpm, None);
    assert!(!serde_json::to_string(&old).unwrap().contains("targetRpm"));
    let new = HeatRow {
        ftst: Some(false),
        fans: vec![FanRow {
            target_rpm: Some(5000.0),
            ..FanRow::default()
        }],
        ..HeatRow::default()
    };
    let wire = serde_json::to_value(&new).unwrap();
    assert_eq!(wire["ftst"], serde_json::json!(false));
    assert_eq!(wire["fans"][0]["targetRpm"], serde_json::json!(5000.0));
}
