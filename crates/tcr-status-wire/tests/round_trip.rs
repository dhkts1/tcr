//! Fixture -> struct -> JSON -> equals fixture, for the shared contract type.
//!
//! This is this crate's half of the byte-identical contract; the other half
//! (`cli::tests::status_contract_fixture_matches_committed` in the main
//! crate) proves `render_accounts_json` itself still emits the committed
//! bytes. Reads the SAME committed fixture both sides already read
//! (`tests/fixtures/status-contract.json`, the "only arrangement in which a
//! silent rename is impossible" per its doc-comment) — never a private copy.

use std::path::Path;

use tcr_status_wire::{AccountStatusRow, SessionRow, UsageRow, UsageTotals, UsageWindow};

fn fixture_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/status-contract.json")
}

/// One of the canonical Codex fixtures, read from where the macOS app's decode tests read
/// it (`apps/macos/Tests/Fixtures/`). The same file, never a copy, for the same reason the
/// contract fixture is shared: two files that must stay equal are the drift a shared file
/// exists to prevent.
fn canonical(name: &str) -> serde_json::Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../apps/macos/Tests/Fixtures")
        .join(name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read the canonical fixture {}: {e}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("the canonical fixture {} is not JSON: {e}", path.display()))
}

/// The ten keys a Codex row may carry that an Anthropic row never does.
const CODEX_ROW_KEYS: [&str; 10] = [
    "quotaWindows",
    "fiveHourWindowMinutes",
    "sevenDayWindowMinutes",
    "limitReachedType",
    "spendControlReached",
    "credits",
    "loginError",
    "codexHome",
    "codexRoute",
    "codexTraffic",
];

/// The canonical status rows, as the bare array the fixture is.
fn canonical_rows() -> Vec<serde_json::Value> {
    match canonical("codex-status.json") {
        serde_json::Value::Array(rows) => rows,
        other => panic!("codex-status.json is a bare array, not {other}"),
    }
}

/// The canonical row with this `name`.
fn canonical_row(name: &str) -> serde_json::Value {
    canonical_rows()
        .into_iter()
        .find(|r| r["name"] == name)
        .unwrap_or_else(|| panic!("codex-status.json carries a row named {name}"))
}

/// Every bucket of a row's `usage` object: `today`, `lastHour`, the two windows when
/// present, and every `todayByModel` value.
fn usage_buckets(
    usage: &mut serde_json::Map<String, serde_json::Value>,
) -> Vec<&mut serde_json::Map<String, serde_json::Value>> {
    let mut buckets = Vec::new();
    for (key, value) in usage.iter_mut() {
        match key.as_str() {
            "today" | "lastHour" | "window" | "sevenDayWindow" => {
                buckets.extend(value.as_object_mut());
            }
            "todayByModel" => {
                if let Some(models) = value.as_object_mut() {
                    buckets.extend(models.values_mut().filter_map(|v| v.as_object_mut()));
                }
            }
            _ => {}
        }
    }
    buckets
}

#[test]
fn fixture_round_trips_through_account_status_row() {
    let committed =
        std::fs::read_to_string(fixture_path()).expect("committed contract fixture is readable");
    let rows: Vec<serde_json::Value> =
        serde_json::from_str(&committed).expect("fixture is a bare JSON array");
    assert!(!rows.is_empty(), "fixture must carry at least one row");

    // Row-at-a-time, per this crate's decode contract — never
    // `serde_json::from_str::<Vec<AccountStatusRow>>` against the whole file.
    let decoded: Vec<AccountStatusRow> = rows
        .iter()
        .cloned()
        .map(|v| AccountStatusRow::from_value(v).expect("fixture row decodes"))
        .collect();

    let reencoded: Vec<serde_json::Value> = decoded
        .into_iter()
        .map(|row| serde_json::to_value(row).expect("row re-encodes"))
        .collect();
    let rendered =
        serde_json::to_string_pretty(&reencoded).expect("re-encoded rows serialize") + "\n";

    assert_eq!(
        committed, rendered,
        "AccountStatusRow does not round-trip the committed fixture byte-for-byte"
    );
}

/// The fixture must carry BOTH shapes of the `usage` key — present and absent —
/// or it only ever proves one of them.
///
/// Absent is not a hypothetical: a client built after usage shipped routinely
/// reads a server built before it, because the binary on disk is rebuilt on
/// merge while the live process keeps serving until someone restarts it. A row
/// with no `usage` key must decode to `None` ("not measured"), never fail the
/// row and never read as a zero-filled day.
#[test]
fn the_fixture_pins_usage_present_and_usage_absent() {
    let committed =
        std::fs::read_to_string(fixture_path()).expect("committed contract fixture is readable");
    let rows: Vec<serde_json::Value> =
        serde_json::from_str(&committed).expect("fixture is a bare JSON array");

    let with_key = rows.iter().filter(|r| r.get("usage").is_some()).count();
    let without_key = rows.len() - with_key;
    assert!(with_key > 0, "no fixture row exercises a present `usage`");
    assert!(
        without_key > 0,
        "no fixture row exercises an absent `usage`"
    );

    for row in &rows {
        let present = row.get("usage").is_some();
        let decoded = AccountStatusRow::from_value(row.clone()).expect("fixture row decodes");
        assert_eq!(
            decoded.usage.is_some(),
            present,
            "a row's decoded usage must match whether the key was there"
        );
    }

    // And the partial-cost case: somewhere in the sample, a day with unpriced
    // requests reports a cost that is a PARTIAL sum, with the count that says
    // so — the reading a client is most likely to present as a whole spend.
    let partial = rows
        .iter()
        .filter_map(|r| AccountStatusRow::from_value(r.clone()).ok())
        .filter_map(|r| r.usage)
        .find(|u| u.today.unpriced_requests > 0)
        .expect("one fixture row must carry unpriced requests");
    assert!(
        partial.today.unpriced_requests < partial.today.requests,
        "the sample's partial day must be partial, not wholly unpriced"
    );
    assert!(
        partial.today.cost_usd.is_some(),
        "a day with SOME priced requests still reports what could be priced"
    );
}

/// `sevenDayOiState` and `sevenDayOiResetAtMs` — the Fable weekly pair — round
/// trip through [`AccountStatusRow`] like their `sevenDay` twins, and a row
/// missing them (an older server's shape) still decodes with both `None`.
#[test]
fn fable_weekly_pair_round_trips_and_decodes_absent() {
    let committed =
        std::fs::read_to_string(fixture_path()).expect("committed contract fixture is readable");
    let rows: Vec<serde_json::Value> =
        serde_json::from_str(&committed).expect("fixture is a bare JSON array");

    // The fixture must carry both a present (non-null) reading and an absent
    // one, or this only ever proves one of the two directions.
    let present = rows
        .iter()
        .find(|r| r["sevenDayOiState"].is_string())
        .expect("fixture carries a row with a learned sevenDayOiState")
        .clone();
    assert!(
        present["sevenDayOiResetAtMs"].is_i64(),
        "the same row also carries a learned sevenDayOiResetAtMs: {present}"
    );
    let decoded = AccountStatusRow::from_value(present.clone()).expect("row decodes");
    assert_eq!(
        decoded.seven_day_oi_state,
        present["sevenDayOiState"].as_str().map(str::to_string)
    );
    assert_eq!(
        decoded.seven_day_oi_reset_at_ms,
        present["sevenDayOiResetAtMs"].as_i64()
    );
    let reencoded = serde_json::to_value(&decoded).expect("row re-encodes");
    assert_eq!(
        reencoded["sevenDayOiState"], present["sevenDayOiState"],
        "sevenDayOiState round-trips byte-for-byte"
    );
    assert_eq!(
        reencoded["sevenDayOiResetAtMs"], present["sevenDayOiResetAtMs"],
        "sevenDayOiResetAtMs round-trips byte-for-byte"
    );

    // A row shaped like an OLDER server's — both keys stripped entirely, not
    // present-as-null — must still decode, reading both as `None`.
    let mut older = present;
    older
        .as_object_mut()
        .expect("row is an object")
        .remove("sevenDayOiState");
    older
        .as_object_mut()
        .expect("row is an object")
        .remove("sevenDayOiResetAtMs");
    let decoded = AccountStatusRow::from_value(older)
        .expect("a row missing the Fable weekly pair still decodes");
    assert_eq!(
        decoded.seven_day_oi_state, None,
        "missing sevenDayOiState decodes as None, not a decode error"
    );
    assert_eq!(
        decoded.seven_day_oi_reset_at_ms, None,
        "missing sevenDayOiResetAtMs decodes as None, not a decode error"
    );
}

/// `quotaUpdatedAtMs`, the time of the newest quota reading from a probe or
/// from response headers, round trips through [`AccountStatusRow`], and a row
/// from an older server that never sent the key still decodes, as `None`.
#[test]
fn quota_updated_at_round_trips_and_decodes_absent() {
    let committed =
        std::fs::read_to_string(fixture_path()).expect("committed contract fixture is readable");
    let rows: Vec<serde_json::Value> =
        serde_json::from_str(&committed).expect("fixture is a bare JSON array");

    let present = rows
        .iter()
        .find(|r| r["quotaUpdatedAtMs"].is_i64())
        .expect("fixture carries a row with a numeric quotaUpdatedAtMs")
        .clone();
    let decoded = AccountStatusRow::from_value(present.clone()).expect("row decodes");
    assert_eq!(
        decoded.quota_updated_at_ms,
        present["quotaUpdatedAtMs"].as_i64()
    );
    let reencoded = serde_json::to_value(&decoded).expect("row re-encodes");
    assert_eq!(reencoded["quotaUpdatedAtMs"], present["quotaUpdatedAtMs"]);

    let mut older = present;
    older
        .as_object_mut()
        .expect("row is an object")
        .remove("quotaUpdatedAtMs");
    let decoded =
        AccountStatusRow::from_value(older).expect("a row missing quotaUpdatedAtMs still decodes");
    assert_eq!(
        decoded.quota_updated_at_ms, None,
        "missing quotaUpdatedAtMs decodes as None, not a decode error"
    );
}

/// Every canonical row, Claude and Codex alike, decodes and re-encodes to exactly the
/// `Value` it came from, and each Codex-only key is skipped when it is absent.
///
/// The round trip is what a client written against this type depends on: TcrBar decodes
/// these same rows, so a key this type dropped, renamed or wrote back as `null` would
/// reach the panel as a different fact. The stripped half is the old-server direction: a
/// row from a server that predates every Codex key must decode with each one unknown,
/// and re-encode with none of them, rather than gaining a `null` or a `false` nobody
/// reported.
#[test]
fn codex_row_keys_round_trip_and_are_skipped_when_absent() {
    let rows = canonical_rows();
    assert_eq!(rows.len(), 10, "codex-status.json carries ten rows");

    for row in &rows {
        let name = row["name"].as_str().expect("every row has a name");
        let decoded = AccountStatusRow::from_value(row.clone())
            .unwrap_or_else(|e| panic!("{name} does not decode: {e}"));
        let reencoded = serde_json::to_value(&decoded).expect("row re-encodes");
        assert_eq!(&reencoded, row, "{name} does not round-trip");
    }

    // The proof covers only the keys some row exercises, so it must exercise all ten.
    for key in CODEX_ROW_KEYS {
        assert!(
            rows.iter().any(|r| r.get(key).is_some()),
            "no canonical row carries {key}"
        );
    }

    let mut stripped = canonical_row("bob@example.com/codex");
    let object = stripped.as_object_mut().expect("a row is an object");
    for key in CODEX_ROW_KEYS {
        object.remove(key);
    }
    let usage = object
        .get_mut("usage")
        .and_then(serde_json::Value::as_object_mut)
        .expect("bob's row carries usage");
    usage.remove("sevenDayWindow");
    for bucket in usage_buckets(usage) {
        bucket.remove("reasoningTokens");
    }

    let decoded =
        AccountStatusRow::from_value(stripped.clone()).expect("a row with no Codex key decodes");
    assert_eq!(
        decoded.quota_windows, None,
        "absent quotaWindows is unknown"
    );
    assert_eq!(decoded.credits, None, "absent credits were never reported");
    assert_eq!(decoded.codex_route, None, "absent codexRoute is unknown");
    assert_eq!(
        decoded.codex_traffic, None,
        "absent codexTraffic is unknown"
    );
    assert!(
        !decoded.spend_control_reached,
        "absent spendControlReached reads as false"
    );
    assert_eq!(
        decoded.usage.as_ref().and_then(|u| u.seven_day_window),
        None,
        "absent sevenDayWindow is unknown"
    );
    assert_eq!(
        serde_json::to_value(&decoded).expect("row re-encodes"),
        stripped,
        "an absent key stays absent: none is written back as null or false"
    );
}

/// `"quotaWindows": []` and no `quotaWindows` key are two different facts, and both
/// survive a round trip.
///
/// `[]` says the plan reported no usage window at all, so the panel draws no window row
/// and says so. An absent key says the windows are unknown, so the panel draws both rows,
/// as it always has. A skip on an empty list would turn the first into the second and
/// draw two empty bars for a plan that has neither.
#[test]
fn empty_quota_windows_is_not_absent() {
    let jo = canonical_row("jo@example.com");
    assert_eq!(
        jo.get("quotaWindows"),
        Some(&serde_json::json!([])),
        "the fixture's jo row carries an empty quotaWindows"
    );
    let decoded = AccountStatusRow::from_value(jo.clone()).expect("jo decodes");
    assert_eq!(
        decoded.quota_windows,
        Some(Vec::new()),
        "[] is a fact: the plan reported no window"
    );
    assert_eq!(
        serde_json::to_value(&decoded)
            .expect("jo re-encodes")
            .get("quotaWindows"),
        Some(&serde_json::json!([])),
        "[] is a fact: the plan reported no window"
    );

    let mut absent = jo;
    absent
        .as_object_mut()
        .expect("a row is an object")
        .remove("quotaWindows");
    let decoded = AccountStatusRow::from_value(absent).expect("a row without the key decodes");
    assert_eq!(
        decoded.quota_windows, None,
        "no key means the windows are unknown"
    );
    assert!(
        serde_json::to_value(&decoded)
            .expect("row re-encodes")
            .get("quotaWindows")
            .is_none(),
        "unknown windows re-encode with no key, never as []"
    );
}

/// A bucket with no reasoning tokens carries no `reasoningTokens` key, alone or flattened
/// into a window, and a bucket written before the key existed decodes it as `0`.
///
/// tcr reads reasoning tokens from Codex responses only, so every Claude bucket is at
/// zero, and a key written at zero would add bytes to every Anthropic row the committed
/// contract fixture pins. The weekly window is skipped when absent for the same reason.
#[test]
fn usage_totals_skip_reasoning_tokens_at_zero() {
    let zero = UsageTotals {
        requests: 3,
        cost_usd: Some(0.5),
        ..Default::default()
    };
    let with = UsageTotals {
        reasoning_tokens: 1_200,
        ..zero
    };

    let value = serde_json::to_value(zero).expect("bucket serializes");
    assert!(
        value.get("reasoningTokens").is_none(),
        "a bucket at zero carries no reasoningTokens: {value}"
    );
    let value = serde_json::to_value(with).expect("bucket serializes");
    assert_eq!(
        value.get("reasoningTokens"),
        Some(&serde_json::json!(1200)),
        "{value}"
    );

    // A window's totals are flattened into it, so the skip must hold there too.
    let value = serde_json::to_value(UsageWindow {
        since: 1,
        totals: zero,
    })
    .expect("window serializes");
    assert!(
        value.get("reasoningTokens").is_none(),
        "a window at zero carries no reasoningTokens: {value}"
    );
    let value = serde_json::to_value(UsageWindow {
        since: 1,
        totals: with,
    })
    .expect("window serializes");
    assert_eq!(
        value.get("reasoningTokens"),
        Some(&serde_json::json!(1200)),
        "{value}"
    );
    assert_eq!(value.get("since"), Some(&serde_json::json!(1)), "{value}");

    // Written as literal text, as a server that predates the key sends it.
    let old: UsageTotals = serde_json::from_str(
        r#"{"requests":3,"inputTokens":10,"cacheCreationTokens":0,"cacheCreation1hTokens":0,"cacheReadTokens":0,"outputTokens":2,"costUsd":0.5,"unpricedRequests":0}"#,
    )
    .expect("a bucket with no reasoningTokens decodes");
    assert_eq!(old.reasoning_tokens, 0, "absent reasoningTokens reads as 0");

    let value = serde_json::to_value(UsageRow::default()).expect("usage serializes");
    assert!(
        value.get("sevenDayWindow").is_none(),
        "no weekly window means no sevenDayWindow key: {value}"
    );
}

/// A status row and a session row each parse with a key this build does not know, at
/// every level that gained a Codex key.
///
/// This is what lets an older TcrBar, or an older `tcr`, read a newer server: serde skips
/// an unknown key unless a type is told to reject them, and this test is what fails the
/// day one of these types is.
#[test]
fn wire_rows_parse_with_an_unknown_key() {
    let later = serde_json::json!({"n": 1});

    let mut carol = canonical_row("carol@example.com");
    carol["addedLater"] = later.clone();
    carol["credits"]["addedLater"] = later.clone();
    carol["codexRoute"]["addedLater"] = later.clone();
    carol["codexTraffic"]["addedLater"] = later.clone();
    carol["usage"]["addedLater"] = later.clone();
    carol["usage"]["sevenDayWindow"]["addedLater"] = later.clone();
    let decoded =
        AccountStatusRow::from_value(carol).expect("a status row with an unknown key decodes");
    assert_eq!(
        decoded.credits.and_then(|c| c.balance).as_deref(),
        Some("12.50"),
        "the known keys beside the unknown one still decode"
    );

    let sessions = canonical("codex-sessions.json");
    let mut codex = sessions["sessions"][1].clone();
    codex["addedLater"] = later.clone();
    codex["tools"]["running"][0]["addedLater"] = later;
    let decoded: SessionRow =
        serde_json::from_value(codex).expect("a session row with an unknown key decodes");
    assert_eq!(decoded.provider.as_deref(), Some("codex"));
    assert_eq!(
        decoded.tools.running[0].command_head.as_deref(),
        Some("cargo build")
    );
}
