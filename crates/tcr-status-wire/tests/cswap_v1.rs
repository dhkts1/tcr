//! `cswap_v1::from_rows` against CodexBar's own parse rules.
//!
//! [`codexbar_parse`] ports the checks of `ClaudeSwapListParser.parse` and
//! `parseRow`/`parseWindow` (CodexBar `ClaudeSwapAccountList.swift:166-306`, read
//! at `5a40ac4`), each of which rejects the WHOLE payload. A projection that
//! fails one of them is a CodexBar that shows no tcr accounts at all, so the
//! rows below cover every shape tcr produces: three windows, no windows, a
//! disabled and a parked row, a login row, a gate this build does not know, and
//! an offline row.
//!
//! The committed fixture `tests/fixtures/cswap-list-v1.json` is the projection
//! of the first six rows, written by hand rather than generated, so the test
//! compares the code against an independent statement of the contract. It is
//! also the file to drop into CodexBar's `ClaudeSwapListParserTests`.

use std::collections::{BTreeMap, HashSet};

use serde_json::Value;
use tcr_status_wire::cswap_v1::{from_rows, UsageStatus};
use tcr_status_wire::AccountStatusRow;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// The ported guards, in the parser's order. `Err` names the guard and the
/// parser's own message, so a failure says which CodexBar check would fire.
fn codexbar_parse(doc: &Value) -> Result<(), String> {
    // 0. The payload is a JSON object (171-175).
    let object = doc.as_object().ok_or("0: not a JSON object")?;
    // 1. `schemaVersion` is a JSON integer, and it is 1 (176-181).
    let version = object
        .get("schemaVersion")
        .and_then(Value::as_i64)
        .ok_or("1: missing schemaVersion")?;
    if version != 1 {
        return Err(format!("1: unsupported schema version {version}"));
    }
    // An `error` object makes the payload an error report (182-186).
    if object.get("error").is_some_and(Value::is_object) {
        return Err("error: the payload is an error envelope".into());
    }
    // 2. `accounts` is an array (187-189).
    let accounts = object
        .get("accounts")
        .and_then(Value::as_array)
        .ok_or("2: missing accounts array")?;
    // 3. `activeAccountNumber` is present (190-192).
    let raw_active = object
        .get("activeAccountNumber")
        .ok_or("3: missing activeAccountNumber")?;
    // 4. `supportsAccountSwitching`, when present, is a boolean (193-198).
    if let Some(switching) = object.get("supportsAccountSwitching") {
        if !switching.is_boolean() {
            return Err("4: supportsAccountSwitching is not a boolean".into());
        }
    }
    // 3, second half. `activeAccountNumber` is a positive slot or null (199-204).
    let active_number: Option<i64> = match raw_active {
        Value::Null => None,
        v => match v.as_i64() {
            Some(n) if n > 0 => Some(n),
            _ => return Err("3: activeAccountNumber is not a numeric slot or null".into()),
        },
    };
    let mut seen = HashSet::new();
    let mut active_slots = Vec::new();
    for raw_row in accounts {
        let row = raw_row
            .as_object()
            .ok_or("row: account row is not an object")?;
        // 5. `number` is an integer > 0 (227-232) and unique (211-213).
        let number = row
            .get("number")
            .and_then(Value::as_i64)
            .ok_or("5: account row has no numeric slot")?;
        if number <= 0 {
            return Err("5: account slot must be positive".into());
        }
        if !seen.insert(number) {
            return Err(format!("5: duplicate account slot {number}"));
        }
        // 6. `active` is a boolean (233-235).
        let active = row
            .get("active")
            .and_then(Value::as_bool)
            .ok_or(format!("6: slot {number} has no active flag"))?;
        if active {
            active_slots.push(number);
        }
        // 7. `usageStatus` is a string (236-238).
        row.get("usageStatus")
            .and_then(Value::as_str)
            .ok_or(format!("7: slot {number} has no usageStatus"))?;
        // 8. A present `fiveHour`/`sevenDay` is an object with a finite `pct`
        //    and, when present, a timestamp `resetsAt` (251-252, 290-306). A
        //    present `null` counts as present: Swift's `guard let raw` passes
        //    NSNull through to the object cast, which throws.
        if let Some(usage) = row.get("usage").and_then(Value::as_object) {
            for name in ["fiveHour", "sevenDay"] {
                if let Some(raw) = usage.get(name) {
                    check_window(raw).map_err(|why| format!("8: slot {number} {name} {why}"))?;
                }
            }
        }
    }
    // 9. The active rows are exactly `[activeAccountNumber]`, or none (216-219).
    if active_slots != active_number.into_iter().collect::<Vec<_>>() {
        return Err("9: active account fields disagree".into());
    }
    Ok(())
}

fn check_window(raw: &Value) -> Result<(), String> {
    let window = raw.as_object().ok_or("window is not an object")?;
    match window.get("pct").and_then(Value::as_f64) {
        Some(pct) if pct.is_finite() => {}
        _ => return Err("percent is not a finite number".into()),
    }
    if let Some(resets_at) = window.get("resetsAt") {
        let text = resets_at.as_str().ok_or("resetsAt is not a timestamp")?;
        OffsetDateTime::parse(text, &Rfc3339).map_err(|_| "resetsAt is not a timestamp")?;
    }
    Ok(())
}

/// `usage.scoped[]` is not fatal in CodexBar (a bad row is dropped, 310-323),
/// so this is stricter than the parser: a dropped Fable bar is still a bug.
fn check_scoped(doc: &Value) {
    for row in doc["accounts"].as_array().expect("accounts") {
        if let Some(scoped) = row["usage"]["scoped"].as_array() {
            for entry in scoped {
                assert!(
                    entry["name"].as_str().is_some_and(|n| !n.trim().is_empty()),
                    "scoped row needs a name: {entry}"
                );
                check_window(entry).expect("scoped window is well formed");
            }
        }
    }
}

const T0: i64 = 1_767_225_600_000; // 2026-01-01T00:00:00Z
const HOUR: i64 = 3_600_000;

/// A live row with every field filled the way a quiet, healthy account reads.
fn base(name: &str) -> AccountStatusRow {
    AccountStatusRow {
        source: "live".into(),
        server_sha: Some("abc1234".into()),
        server_dirty: Some(false),
        http1_only: false,
        name: name.into(),
        priority: 0,
        status: "active".into(),
        disabled: false,
        control: false,
        quota: None,
        quota_state: "ok".into(),
        gate: "ok".into(),
        five_hour: None,
        seven_day: None,
        seven_day_oi: None,
        five_hour_state: None,
        seven_day_state: None,
        seven_day_oi_state: None,
        five_hour_reset_at_ms: None,
        seven_day_reset_at_ms: None,
        seven_day_oi_reset_at_ms: None,
        requests: Some(0),
        input_tokens: Some(0),
        output_tokens: Some(0),
        cache_read_tokens: Some(0),
        cache_hit_ratio: None,
        last_probe_ms: None,
        quota_updated_at_ms: None,
        probe_status: "ok".into(),
        probe_error: None,
        stream_error_count: Some(0),
        last_stream_error: None,
        held: Vec::new(),
        free_at_ms: None,
        seconds_until_free: None,
        rate_limited_until_ms: None,
        groups: Vec::new(),
        reserved_groups: Vec::new(),
        parked_groups: Vec::new(),
        control_allowed_groups: Vec::new(),
        group_colors: BTreeMap::new(),
        cache_creation_tokens: Some(0),
        usage: None,
        provider: None,
        quota_windows: None,
        five_hour_window_minutes: None,
        seven_day_window_minutes: None,
        limit_reached_type: None,
        spend_control_reached: false,
        credits: None,
        login_error: None,
        codex_home: None,
        token_sync_pending: false,
        codex_route: None,
        codex_traffic: None,
        organization_type: None,
        rate_limit_tier: None,
        seat_tier: None,
        plan: None,
        org_uuid: None,
        org_name: None,
    }
}

/// The six rows the committed fixture is the projection of, in fixture order.
fn fixture_rows() -> Vec<AccountStatusRow> {
    vec![
        // All three windows, an org suffix on the name, a reading time.
        AccountStatusRow {
            five_hour: Some(0.4),
            five_hour_reset_at_ms: Some(T0 + 4 * HOUR),
            seven_day: Some(0.6),
            seven_day_reset_at_ms: Some(T0 + 168 * HOUR),
            seven_day_oi: Some(0.1),
            seven_day_oi_reset_at_ms: Some(T0 + 168 * HOUR),
            last_probe_ms: Some(T0 - HOUR),
            quota_updated_at_ms: Some(T0 - HOUR),
            org_name: Some("Example Team".into()),
            ..base("alice@example.com/example-team")
        },
        // Never probed and nothing known: the shape an API-key account has.
        AccountStatusRow {
            probe_status: "never".into(),
            ..base("bob@example.com")
        },
        // Disabled by hand, with a 5-hour reading and no reset learned.
        AccountStatusRow {
            disabled: true,
            gate: "disabled".into(),
            five_hour: Some(0.2),
            ..base("carol@example.com")
        },
        // Parked through a group, last probe failed.
        AccountStatusRow {
            parked_groups: vec!["night".into()],
            groups: vec!["night".into()],
            gate: "parked".into(),
            probe_status: "error".into(),
            seven_day: Some(0.5),
            seven_day_reset_at_ms: Some(T0 + 24 * HOUR),
            ..base("dave@example.com")
        },
        // Needs a login.
        AccountStatusRow {
            gate: "login".into(),
            probe_status: "error".into(),
            ..base("erin@example.com")
        },
        // A gate this build does not know yet, over a good probe.
        AccountStatusRow {
            gate: "refused".into(),
            five_hour: Some(0.1),
            ..base("frank@example.com")
        },
    ]
}

/// One guard case: the guard number expected to fire, and how to break the payload.
type GuardCase = (&'static str, fn(&mut Value));

fn project(rows: &[AccountStatusRow]) -> Value {
    serde_json::to_value(from_rows(rows)).expect("the projection serializes")
}

#[test]
fn the_projection_passes_every_codexbar_guard() {
    let doc = project(&fixture_rows());
    codexbar_parse(&doc).unwrap_or_else(|why| panic!("CodexBar would reject it: {why}\n{doc:#}"));
    check_scoped(&doc);

    assert!(
        doc.as_object()
            .expect("object")
            .contains_key("activeAccountNumber")
            && doc["activeAccountNumber"].is_null(),
        "the key is present and null"
    );
    assert_eq!(doc["supportsAccountSwitching"], false);
    let numbers: Vec<i64> = doc["accounts"]
        .as_array()
        .expect("accounts")
        .iter()
        .map(|r| r["number"].as_i64().expect("number"))
        .collect();
    assert_eq!(
        numbers,
        vec![1, 2, 3, 4, 5, 6],
        "1-based, in the order given"
    );
}

/// The guard port itself has to say no. Each mutation is one the projection
/// could plausibly make; each must be refused by the guard it names.
#[test]
fn the_guard_port_refuses_what_codexbar_refuses() {
    let good = project(&fixture_rows());
    let cases: [GuardCase; 8] = [
        ("1:", |d| d["schemaVersion"] = 2.into()),
        ("2:", |d| d["accounts"] = Value::Null),
        ("3:", |d| {
            d.as_object_mut()
                .expect("object")
                .remove("activeAccountNumber");
        }),
        ("3:", |d| d["activeAccountNumber"] = 0.into()),
        ("4:", |d| d["supportsAccountSwitching"] = "no".into()),
        ("5:", |d| d["accounts"][1]["number"] = 1.into()),
        ("8:", |d| {
            d["accounts"][0]["usage"]["fiveHour"] = Value::Null
        }),
        ("9:", |d| d["accounts"][0]["active"] = true.into()),
    ];
    for (guard, mutate) in cases {
        let mut doc = good.clone();
        mutate(&mut doc);
        let why = codexbar_parse(&doc).expect_err("a mutated payload must be refused");
        assert!(why.starts_with(guard), "expected guard {guard}, got {why}");
    }
}

#[test]
fn usage_status_follows_gate_then_probe() {
    let doc = project(&fixture_rows());
    let statuses: Vec<&str> = doc["accounts"]
        .as_array()
        .expect("accounts")
        .iter()
        .map(|r| r["usageStatus"].as_str().expect("status"))
        .collect();
    assert_eq!(
        statuses,
        vec![
            "ok",
            "unavailable",
            "ok",
            "unavailable",
            "relogin_required",
            "unavailable"
        ]
    );
}

#[test]
fn an_offline_row_is_unavailable_whatever_else_it_says() {
    let offline = AccountStatusRow {
        source: "offline".into(),
        five_hour: Some(0.3),
        ..base("alice@example.com")
    };
    let login = AccountStatusRow {
        source: "offline".into(),
        gate: "login".into(),
        ..base("bob@example.com")
    };
    let list = from_rows(&[offline, login]);
    assert!(list
        .accounts
        .iter()
        .all(|a| a.usage_status == UsageStatus::Unavailable));
}

#[test]
fn the_committed_fixture_is_the_projection_and_codexbar_accepts_it() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/cswap-list-v1.json");
    let committed = std::fs::read_to_string(&path).expect("committed fixture is readable");
    let fixture: Value = serde_json::from_str(&committed).expect("fixture is JSON");
    codexbar_parse(&fixture).unwrap_or_else(|why| panic!("the fixture fails a guard: {why}"));
    check_scoped(&fixture);
    let rendered = serde_json::to_string_pretty(&from_rows(&fixture_rows()))
        .expect("the projection serializes")
        + "\n";
    assert_eq!(
        rendered, committed,
        "the projection drifted from the committed fixture"
    );
}

/// `usageFetchedAt` is when the bars were read, not when the probe last ran. A
/// failed probe stamps `lastProbeMs` and reads nothing, so an account whose
/// probe broke an hour ago over bars from a day ago must report the day-old
/// time. With no reading time the key is left out: unknown, not fresh.
#[test]
fn usage_fetched_at_is_the_reading_time_not_the_probe_time() {
    let failed_probe = AccountStatusRow {
        probe_status: "error".into(),
        five_hour: Some(0.4),
        last_probe_ms: Some(T0 - HOUR),
        quota_updated_at_ms: Some(T0 - 24 * HOUR),
        ..base("alice@example.com")
    };
    let never_read = AccountStatusRow {
        probe_status: "error".into(),
        last_probe_ms: Some(T0 - HOUR),
        quota_updated_at_ms: None,
        ..base("bob@example.com")
    };
    let doc = project(&[failed_probe, never_read]);
    assert_eq!(
        doc["accounts"][0]["usageFetchedAt"], "2025-12-31T00:00:00Z",
        "the time of the old reading, not of the failed probe"
    );
    assert!(
        !doc["accounts"][1]
            .as_object()
            .expect("row")
            .contains_key("usageFetchedAt"),
        "no reading time means no key: {:#}",
        doc["accounts"][1]
    );
}
