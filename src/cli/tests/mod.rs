//! Tests for `tcr::cli`, one file per child module they exercise. This file holds the
//! imports they share and the fixtures and helpers more than one of them uses.

use std::sync::Arc;
use std::time::Duration;
use time::OffsetDateTime;

use super::accounts::{priority_message, priority_value, write_control_account};
use super::fan::{
    FanSent, FanWait, accepted_command, fan_succeeded, fan_wait, fan_word, render_fan_text,
};
use super::groups::{
    ROUTE_BLOCK_CONTROL_ONLY, ReservePlan, codex_group_add_notice, control_account_group_notice,
    low_contrast_color_warning, plan_reserve, render_groups_json, render_groups_text,
};
use super::list_v1::{overlay_live_rows, render_list_v1};
use super::live_edit::{
    CONFIG_SET_WORDS, CONTROL_WORDS, ENABLE_WORDS, Live, LiveSite, LiveWords, PRIORITY_WORDS,
    REMOVE_WORDS, Stale,
};
use super::live_reads::{
    RoutingReadError, fetch_routing_body, payload_carries_sessions, render_sessions_json,
    render_sessions_text,
};
use super::render::{
    CACHE_SIGNAL_FLOOR_REQUESTS, account_status_rows, account_status_rows_at, claude_resets_token,
    codex_tail, format_reset_clock, format_reset_countdown, held_suffix, render_accounts_json,
    status_rows_json,
};
use super::status::{
    StatusText, build_fields, codex_traffic_notes, codex_wire_facts, eligible_token,
    holds_provider, relogin_hint_lines, render_status_text, skew_report, status_holds_provider,
};
use crate::build_info::{self, BuildInfo};
use crate::config::Account;
use crate::live_client::{
    LiveControlError, LiveStatusError, RESTART_HINT, Unreached, fetch_live_status,
    parse_live_status, send_loopback,
};
use crate::log_err::LogErr as _;
use crate::manager::Manager;
use crate::oauth::{NoRefresh, TokenRefresher};
use crate::probe::{ProbeStatus, UsageProber};
use crate::provider::Provider;
use crate::stats::{AccountSnapshot, CodexFacts, QuotaState, StatsSnapshot};
use crate::status::{STATUS_KIND, StatusPayload};

mod account_query;
mod accounts;
mod fan;
mod groups;
mod list_v1;
mod live_client;
mod live_edit;
mod live_reads;
mod load_save;
mod render;
mod render_codex;
mod render_contract;
mod settings;
mod status;

use super::*;
use crate::oauth::{OAuthError, RefreshFuture};
use crate::probe::{ProbeFuture, Usage, UsageBucket};
use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};

/// `snapshot` as `tcr status --json` renders it from `source`, with no server build, no
/// `http1Only`, no control account and no group colours.
fn status_json(snapshot: &StatsSnapshot, thresholds: &[f64], source: StatusSource) -> String {
    render_accounts_json(
        snapshot,
        thresholds,
        source,
        None,
        false,
        None,
        &Default::default(),
    )
}

/// The row named `name` in the `tcr status --json` text `json`.
fn status_row(json: &str, name: &str) -> serde_json::Value {
    let rows: Vec<serde_json::Value> = serde_json::from_str(json).expect("json is a bare array");
    rows.into_iter()
        .find(|r| r["name"] == name)
        .unwrap_or_else(|| panic!("no row named {name} in {json}"))
}

/// A unique temp config path per call so parallel tests never collide.
fn temp_config_path(tag: &str) -> std::path::PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "tcr-cli-test-{tag}-{}-{seq}.json",
        std::process::id()
    ))
}

/// Write `json` to a fresh temp path and return it.
fn write_config(tag: &str, json: &str) -> std::path::PathBuf {
    let path = temp_config_path(tag);
    fs::write(&path, json).unwrap();
    path
}

const TWO_ACCOUNTS: &str = r#"{
      "proxy": { "port": 3456 },
      "quotaProbeSeconds": 120,
      "routes": [{ "name": "r1", "match": "*fable*" }],
      "accounts": [
        { "name": "alice@example.com", "type": "oauth", "orgName": "Org A",
          "orgUuid": "uuid-a", "accessToken": "at-a", "refreshToken": "rt-a",
          "expiresAt": 1893456000000, "priority": 0 },
        { "name": "bob@example.com", "type": "oauth", "orgName": "Org B",
          "orgUuid": "uuid-b", "accessToken": "at-b", "refreshToken": "rt-b",
          "expiresAt": 1893456000000, "priority": 1 }
      ]
    }"#;

fn load(path: &Path) -> Config {
    config::load(path).unwrap()
}

// --- load_for_edit refuses a config with a quarantined account ---------

/// A fixture whose port is a scratch listener's, bound here and handed back
/// for the test to serve on.
async fn config_on_a_scratch_port(tag: &str) -> (std::path::PathBuf, tokio::net::TcpListener) {
    let (listener, port) = crate::test_support::loopback::listener().await;
    let path = config_on_a_dead_port(tag).await;
    point_at_port(&path, port);
    (path, listener)
}

/// Serve the config at `path` on `listener` with its second account renamed to
/// `alice@example.com`, so the RUNNING fleet holds two alices while the file keeps unique
/// names (the loader guarantees that). Answers the file's text from before the proxy started.
fn serve_a_fleet_with_two_alices(path: &Path, listener: tokio::net::TcpListener) -> String {
    let mut config = load(path);
    config.accounts[1].name = "alice@example.com".to_string();
    let before = fs::read_to_string(path).unwrap();
    crate::test_support::live_proxy::serve(listener, config, Some(path.to_path_buf()));
    before
}

/// Rewrite the config file at `path` so its `proxy.port` is `port`, the way a CLI pointed at a
/// test's live proxy finds it.
fn point_at_port(path: &Path, port: u16) {
    let raw = fs::read_to_string(path).unwrap();
    let mut doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
    doc["proxy"]["port"] = serde_json::json!(port);
    fs::write(path, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
}

/// alice (Claude) and bob (Codex) on a port nothing serves: an edit asks
/// whether a server is running on the config's port, and without one that is
/// 3456, where a live tcr may be serving. `bob_extra` is merged into bob's row.
async fn alice_and_codex_bob(tag: &str, bob_extra: serde_json::Value) -> std::path::PathBuf {
    let port = crate::test_support::loopback::unused_port();
    let mut bob = serde_json::json!({
        "name": "bob@example.com/codex", "provider": "codex", "type": "oauth",
        "accessToken": "at-b", "refreshToken": "rt-b", "credentialVersion": 2
    });
    if let (Some(row), Some(extra)) = (bob.as_object_mut(), bob_extra.as_object()) {
        row.extend(extra.clone());
    }
    let json = serde_json::json!({
        "proxy": { "port": port },
        "accounts": [
            { "name": "alice@example.com", "type": "oauth", "accessToken": "at-a",
              "refreshToken": "rt-a" },
            bob
        ]
    });
    assert_ne!(json["proxy"]["port"], serde_json::json!(3456));
    write_config(tag, &json.to_string())
}

/// `TWO_ACCOUNTS` pointed at a port nothing serves, written to a temp file.
///
/// The port is `test_support::loopback::unused_port`, which never draws 3456: `TWO_ACCOUNTS`
/// names 3456 and this substitutes it away precisely so a test can never POST an
/// account-control command at the REAL proxy, and a draw of 3456 would make the
/// substitution a no-op.
///
/// Load-bearing safety, not tidiness: `set_enabled` is live-first now, and
/// `TWO_ACCOUNTS` names port **3456** — the port a REAL tcr serves Gil's fleet
/// on. A test left on that port would POST an account-control command at the
/// live proxy. The substitution is asserted below because a silent no-op here
/// would aim every one of these tests at the running server.
async fn config_on_a_dead_port(tag: &str) -> std::path::PathBuf {
    let port = crate::test_support::loopback::unused_port();
    let json = TWO_ACCOUNTS.replace("\"port\": 3456 }", &format!("\"port\": {port} }}"));
    // Compare the PARSED value, not the text: a substring check for
    // `"port": 3456` also matched a drawn port of 34560–34569 and tripped
    // this guard on a healthy substitution (about one Linux run in 3,000).
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(
        parsed["proxy"]["port"],
        serde_json::json!(port),
        "the port substitution must apply, or this test talks to the live proxy"
    );
    write_config(tag, &json)
}

fn load_from(json: &str) -> Config {
    serde_json::from_str(json).unwrap()
}

/// A prober that returns a fixed 5-hour bar for any token — lets a snapshot
/// carry a known quota% without touching the network.
struct FixedProber {
    util: f64,
}
impl UsageProber for FixedProber {
    fn probe(&self, _credential: crate::probe::ProbeCredential) -> ProbeFuture {
        let util = self.util;
        Box::pin(async move {
            Ok(Usage {
                five_hour: crate::probe::WindowReading::Read(UsageBucket {
                    utilization: Some(util),
                    reset_at_ms: Some(crate::now_ms() + 3_600_000),
                }),
                seven_day: None,
                seven_day_oi: None,
                extra_usage_usd: None,
                codex: None,
                claude_resets: None,
            })
        })
    }
}

// --- tcr list --json ----------------------------------------------------

/// A prober that always fails as if the usage endpoint rejected the token
/// (HTTP 401) — the shape an EXPIRED account's stale access token gets once
/// [`NoRefresh`] has declined to refresh it.
struct RejectingProber;
impl UsageProber for RejectingProber {
    fn probe(&self, _credential: crate::probe::ProbeCredential) -> ProbeFuture {
        Box::pin(async {
            Err(crate::probe::ProbeError {
                status: Some(401),
                message: "HTTP 401: token expired".to_string(),
                retry_after_secs: None,
            })
        })
    }
}

/// One OAuth account whose token expired in 1970 — probing it would, before the
/// fix, trigger a real OAuth refresh that revokes the running server's copy.
const EXPIRED_ACCOUNT: &str = r#"{
      "proxy": { "port": 3456 },
      "accounts": [
        { "name": "stale@example.com", "type": "oauth", "orgName": "Org X",
          "orgUuid": "uuid-x", "accessToken": "at-stale", "refreshToken": "rt-stale",
          "expiresAt": 1000, "priority": 0 }
      ]
    }"#;

/// Path of the fixture BOTH sides read. One file, never a copy: the whole
/// point is that the Swift decoder and this renderer cannot drift apart, and
/// two files that must stay equal are the drift this exists to prevent.
/// `apps/macos/Tests/TcrBarTests/RealWorldDecodeTests.swift` reaches the same
/// path from `#filePath`.
fn status_contract_fixture_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/status-contract.json")
}

/// One fully-specified account row for the fixture.
///
/// Every field is passed explicitly rather than defaulted, so a new field on
/// [`AccountSnapshot`] breaks this call site and forces a decision about what
/// the contract sample should say about it.
#[expect(
    clippy::too_many_arguments,
    reason = "a test builder whose arguments are the row's fields"
)]
fn contract_account(
    name: &str,
    priority: i64,
    status: &str,
    disabled: bool,
    five_hour: Option<f64>,
    five_hour_reset: Option<OffsetDateTime>,
    seven_day: Option<f64>,
    seven_day_reset: Option<OffsetDateTime>,
    seven_day_oi: Option<f64>,
    seven_day_oi_reset: Option<OffsetDateTime>,
    probe_status: ProbeStatus,
    probe_error: Option<&str>,
    quota_state: QuotaState,
    groups: &[&str],
    usage: Option<tcr_status_wire::UsageRow>,
    // `plan` is `(organizationType, rateLimitTier, seatTier)` — the three
    // raw strings, exactly as the profile endpoint spells them. `org` is
    // `(orgUuid, orgName)`, the org a client shows beside this row.
    // Grouped rather than five more
    // positional `Option<&str>` arguments, which at this arity is a call
    // site nobody can read.
    plan: (Option<&str>, Option<&str>, Option<&str>),
    org: (Option<&str>, Option<&str>),
) -> AccountSnapshot {
    AccountSnapshot {
        slot: 0,
        name: name.to_string(),
        // Anthropic unless the sample says otherwise: its one Codex row
        // overrides this after construction, as row 2 does `free_at`.
        provider: Provider::Anthropic,
        organization_type: plan.0.map(str::to_string),
        rate_limit_tier: plan.1.map(str::to_string),
        seat_tier: plan.2.map(str::to_string),
        plan_display_name: None,
        org_uuid: org.0.map(str::to_string),
        org_name: org.1.map(str::to_string),
        priority,
        status: status.to_string(),
        disabled,
        five_hour,
        five_hour_reset,
        five_hour_absent_on_plan: false,
        seven_day,
        seven_day_reset,
        seven_day_oi,
        seven_day_oi_reset,
        requests: if five_hour.is_some() { 102 } else { 0 },
        input_tokens: if five_hour.is_some() { 8_000_000 } else { 0 },
        output_tokens: if five_hour.is_some() { 31_860 } else { 0 },
        cache_read_tokens: if five_hour.is_some() { 6_000_000 } else { 0 },
        // Chosen so the four input dimensions reconcile: `inputTokens` is
        // the quota counter and equals base + cache creation + cache reads,
        // so 8,000,000 = 400,000 + 1,600,000 + 6,000,000. A fixture whose
        // numbers do not add up teaches every reader the wrong arithmetic.
        cache_creation_tokens: if five_hour.is_some() { 1_600_000 } else { 0 },
        last_used: None,
        rate_limited_until: None,
        probe_status,
        last_probe: None,
        quota_updated_at: None,
        probe_error: probe_error.map(str::to_string),
        quota_state,
        gate: crate::stats::GateReason::Ok,
        free_at: None,
        stream_error_count: if five_hour.is_some() { 2 } else { 0 },
        last_stream_error: if five_hour.is_some() {
            Some("overloaded_error".to_string())
        } else {
            None
        },
        groups: groups.iter().map(ToString::to_string).collect(),
        reserved_groups: Vec::new(),
        parked_groups: Vec::new(),
        control_allowed_groups: Vec::new(),
        usage,
        codex: Default::default(),
    }
}

/// One model's slice of a fixture row's usage, priced by the SAME function
/// production prices with.
///
/// A cost typed into a fixture is a claim about the pricing table that
/// nothing checks: change a rate and the fixture goes on asserting the old
/// dollar figure is what this proxy produces. Running the real
/// [`crate::pricing`] path means the committed bytes are what the code
/// actually computes, and a rate change shows up as a fixture diff — which
/// is most of the point of committing one.
fn contract_totals(
    model: &str,
    requests: u64,
    input: u64,
    cache_5m: u64,
    cache_1h: u64,
    cache_read: u64,
    output: u64,
) -> tcr_status_wire::UsageTotals {
    let price = crate::pricing::PricingTable::default().lookup(model);
    tcr_status_wire::UsageTotals {
        requests,
        input_tokens: input,
        // BOTH TTLs, exactly as `Totals::to_wire` emits it — the same
        // quantity the row-level `cacheCreationTokens` carries, with the
        // 1-hour part broken out beside it.
        cache_creation_tokens: cache_5m + cache_1h,
        cache_creation_1h_tokens: cache_1h,
        cache_read_tokens: cache_read,
        output_tokens: output,
        cost_usd: price.map(|price| {
            crate::pricing::cost_nanos(&price, input, cache_5m, cache_1h, cache_read, output) as f64
                / 1e9
        }),
        unpriced_requests: if price.is_some() { 0 } else { requests },
        reasoning_tokens: 0,
    }
}

/// Add `other` into `totals`, the way the tracker's own buckets sum.
/// `costUsd` is `None` only when NOTHING in the sum could be priced.
fn contract_add(
    totals: tcr_status_wire::UsageTotals,
    other: tcr_status_wire::UsageTotals,
) -> tcr_status_wire::UsageTotals {
    let requests = totals.requests + other.requests;
    let unpriced_requests = totals.unpriced_requests + other.unpriced_requests;
    tcr_status_wire::UsageTotals {
        requests,
        input_tokens: totals.input_tokens + other.input_tokens,
        cache_creation_tokens: totals.cache_creation_tokens + other.cache_creation_tokens,
        cache_creation_1h_tokens: totals.cache_creation_1h_tokens + other.cache_creation_1h_tokens,
        cache_read_tokens: totals.cache_read_tokens + other.cache_read_tokens,
        output_tokens: totals.output_tokens + other.output_tokens,
        // Summed in NANODOLLARS, then converted once — exactly what the
        // tracker does. Adding the two dollar figures as `f64` instead
        // yields 14.165700000000001 for a total the production path renders
        // as 14.1657, and a fixture that disagrees with production in its
        // last bit is a fixture that will be "fixed" by someone rounding.
        cost_usd: if requests > 0 && unpriced_requests >= requests {
            None
        } else {
            let nanos = |usd: Option<f64>| (usd.unwrap_or(0.0) * 1e9).round() as u64;
            Some((nanos(totals.cost_usd) + nanos(other.cost_usd)) as f64 / 1e9)
        },
        unpriced_requests,
        reasoning_tokens: totals.reasoning_tokens + other.reasoning_tokens,
    }
}

/// A fixture row's `usage`, built from a mix of two models.
///
/// The token counts reconcile with the row-level counters
/// [`contract_account`] sets: 102 requests, 400,000 base input, 1,600,000
/// cache creation (1,200,000 at 5 minutes plus 400,000 at an hour, and the
/// nested `cacheCreationTokens` carries that same 1,600,000 the row-level
/// key does), 6,000,000 cache reads and 31,860 output. `lastHour` is a subset of
/// `window`, which is a subset of `today` — the containment any reader will
/// assume, so the sample had better honour it.
///
/// `second_model` is what makes the two priced rows differ from the third:
/// pass a model the table has no published rate for and `costUsd` becomes a
/// PARTIAL total with `unpricedRequests` saying how much is missing, which
/// is the case a client renderer is most likely to get wrong.
fn contract_usage(second_model: &str) -> tcr_status_wire::UsageRow {
    let opus = |requests: u64| {
        contract_totals(
            "claude-opus-5",
            requests,
            4_000 * requests,
            12_000 * requests,
            4_000 * requests,
            60_000 * requests,
            302 * requests,
        )
    };
    let other = |requests: u64| {
        contract_totals(
            second_model,
            requests,
            3_750 * requests,
            11_250 * requests,
            3_750 * requests,
            56_250 * requests,
            335 * requests,
        )
    };
    tcr_status_wire::UsageRow {
        today: contract_add(opus(70), other(32)),
        window: Some(tcr_status_wire::UsageWindow {
            // The 5-hour reset every fixture row carries, minus five hours.
            since: 1_767_225_600_000 - crate::usage::FIVE_HOURS_MS,
            totals: contract_add(opus(28), other(12)),
        }),
        last_hour: contract_add(opus(8), other(4)),
        today_by_model: [
            ("claude-opus-5".to_string(), opus(70)),
            (second_model.to_string(), other(32)),
        ]
        .into_iter()
        .collect(),
        seven_day_window: None,
    }
}

/// A fixture row's `usage` for an account that has served nothing: what the
/// usage tracker reports for one, every bucket a MEASURED zero (`costUsd:
/// 0.0`, never the `null` that means "could not be priced") and no `window`,
/// because no five-hour reset was ever learned to name the window's start.
fn contract_idle_usage() -> tcr_status_wire::UsageRow {
    let idle = tcr_status_wire::UsageTotals {
        cost_usd: Some(0.0),
        ..Default::default()
    };
    tcr_status_wire::UsageRow {
        today: idle,
        window: None,
        last_hour: idle,
        today_by_model: std::collections::BTreeMap::new(),
        seven_day_window: None,
    }
}

/// A fixed instant in the PAST, so the two clock-derived fields render
/// deterministically.
///
/// `minutesUntilReset` is `(reset - now).whole_minutes().max(0)` and
/// `secondsUntilFree` is dropped entirely once `free_at` has elapsed, so
/// every reset instant here is historical and both render as their clamped /
/// absent value. That buys a byte-for-byte comparison against raw renderer
/// output with **no normalisation layer** — and a normalisation layer is
/// exactly where a contract pin rots, because it is the one part of the
/// comparison nothing checks.
fn fixture_instant(unix_ms: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(unix_ms) * 1_000_000)
        .expect("fixture instant is a valid timestamp")
}

/// The snapshot the fixture is rendered from. Obviously-fake data only —
/// this repository is public.
fn status_contract_snapshot() -> (StatsSnapshot, Vec<f64>) {
    // 2026-01-01T00:00:00Z and 2026-01-02T00:00:00Z, both long past.
    let five_hour_reset = fixture_instant(1_767_225_600_000);
    let seven_day_reset = fixture_instant(1_767_312_000_000);

    let accounts = vec![
        // `ok`: probed, measured, in rotation — the ordinary row.
        contract_account(
            "alice@example.com",
            0,
            "active",
            false,
            Some(0.04),
            Some(five_hour_reset),
            Some(0.01),
            Some(seven_day_reset),
            Some(0.0),
            Some(seven_day_reset),
            ProbeStatus::Ok,
            None,
            QuotaState::Normal,
            // Non-empty and multi-membership — the only row the Swift decode
            // test has real group data to read, and the one the greppable
            // `groups=codereview,dev` text-line assertion targets.
            &["codereview", "dev"],
            // Every request priced: `costUsd` is a complete figure and
            // `unpricedRequests` is 0.
            Some(contract_usage("claude-sonnet-5")),
            // `Max 20x` — the multiplier suffix, and the plan the fleet's
            // personal rows carry.
            (Some("claude_max"), Some("default_claude_max_20x"), None),
            (
                Some("11111111-1111-1111-1111-111111111111"),
                Some("Example Personal"),
            ),
        ),
        // `near`: at threshold, so `held` carries a window — the only row
        // that exercises the nested object.
        contract_account(
            "bob@example.com",
            1,
            "active",
            false,
            Some(0.95),
            Some(five_hour_reset),
            Some(0.91),
            Some(seven_day_reset),
            Some(0.10),
            Some(seven_day_reset),
            ProbeStatus::RateLimited,
            None,
            QuotaState::NearLimit,
            &[],
            // A model the built-in table has no published rate for, so a
            // THIRD of this row's requests are unpriced: `costUsd` is a
            // partial total and `unpricedRequests` says so. The case a
            // client renderer is most likely to get wrong — silently
            // presenting a partial sum as the whole spend.
            Some(contract_usage("claude-sonnet-4-5-20250929")),
            // `Team 5x` — a premium Team seat, where the multiplier
            // OUTRANKS the seat (`team_tier_1` is not what shows).
            (
                Some("claude_team"),
                Some("default_claude_max_5x"),
                Some("team_tier_1"),
            ),
            (
                Some("22222222-2222-2222-2222-222222222222"),
                Some("Example Team"),
            ),
        ),
        // `spent`: fully consumed, and carrying a probe error string.
        contract_account(
            "carol@example.com",
            2,
            "throttled",
            false,
            Some(1.0),
            Some(five_hour_reset),
            Some(1.0),
            Some(seven_day_reset),
            Some(0.5),
            Some(seven_day_reset),
            ProbeStatus::Error,
            Some("probe failed: connection reset"),
            QuotaState::Exhausted,
            &[],
            Some(contract_usage("claude-sonnet-5")),
            // `Team Standard` — a standard seat, whose tier names no
            // multiplier, so the SEAT is the suffix. Same org as bob above,
            // which is the pairing that makes `orgUuid` worth carrying.
            (
                Some("claude_team"),
                Some("default_raven"),
                Some("team_standard"),
            ),
            (
                Some("22222222-2222-2222-2222-222222222222"),
                Some("Example Team"),
            ),
        ),
        // Never probed AND disabled: the four quota fractions and
        // `cacheHitRatio` are all null. This row is the one that shipped a
        // decode crash — `valueNotFound … Path: [2].quota`. Also the
        // unlabelled row: `groups` must render `[]`, never `null`.
        contract_account(
            "dave@example.com",
            3,
            "active",
            true,
            None,
            None,
            None,
            None,
            None,
            None,
            ProbeStatus::Never,
            None,
            QuotaState::Normal,
            &[],
            // NO usage object at all — the shape a client gets from a
            // server built before usage existed. The key is absent, not
            // null-valued and not zero-filled, so both decoders are pinned
            // against the skew that actually happens here: the binary on
            // disk is rebuilt on merge while the live process keeps serving
            // until someone restarts it.
            None,
            // NEVER PROFILED: all three plan keys null and `plan` null with
            // them. The row both decoders must render as "plan unknown"
            // rather than inventing one — and the shape every account has
            // until its first probe backfills it.
            (None, None, None),
            (None, None),
        ),
        // A Codex account, under the name the import gives a ChatGPT login
        // whose email an Anthropic row already holds (`<email>/codex`). Its
        // plan is the raw `chatgpt_plan_type` claim, labelled `ChatGPT
        // Pro`, with no rate-limit tier, no seat and no org: a ChatGPT
        // workspace is named by its account id alone. Probed, with a
        // WEEKLY reading only, which is all the Codex backend reports, and
        // idle, so its usage is the measured zero. Disabled, so it adds
        // nothing to the capacity counts a panel derives from this sample:
        // a panel that predates `provider` would count an enabled Codex row
        // as Claude capacity, and the Swift suite pins those counts.
        contract_account(
            "bob@example.com/codex",
            4,
            "active",
            true,
            None,
            None,
            Some(0.10),
            Some(seven_day_reset),
            None,
            None,
            ProbeStatus::Ok,
            None,
            QuotaState::Normal,
            &[],
            Some(contract_idle_usage()),
            (Some("pro"), None, None),
            (None, None),
        ),
    ];
    let mut accounts = accounts;
    // `freeAtMs` and `rateLimitedUntilMs` must be pinned as *numbers*
    // somewhere in the sample, or the fixture would only ever prove they can
    // be null — which a deleted key also looks like. Both instants are past,
    // so `secondsUntilFree` (the one clock-derived companion) stays absent
    // and the render stays deterministic.
    accounts[2].free_at = Some(fixture_instant(1_767_312_000_000));
    accounts[2].rate_limited_until = Some(fixture_instant(1_767_225_600_000));
    // Same reason for `quotaUpdatedAtMs`: a number on the ordinary row, null
    // on the others, so both shapes are pinned.
    accounts[0].quota_updated_at = Some(fixture_instant(1_767_225_600_000));
    // The sample's one non-Anthropic row, so it pins both shapes of the
    // `provider` key: absent on the four rows above, `"codex"` here.
    accounts[4].provider = Provider::Codex;
    accounts[4].codex.reset_credits = Some(tcr_status_wire::ResetCredits {
        available_count: 3,
        earliest_expiry_ms: Some(1_800_000_000_000),
    });

    let thresholds = vec![0.9; accounts.len()];
    (
        StatsSnapshot {
            accounts,
            current: Some(0),
            current_by_provider: crate::stats::CurrentByProvider::default(),
            recent: Vec::new(),
            sessions: Vec::new(),
            wire_sessions: Vec::new(),
            wire_sessions_summary: tcr_status_wire::SessionsSummary::default(),
        },
        thresholds,
    )
}

/// The exact bytes the fixture file must hold.
fn status_contract_rendered() -> String {
    let (snapshot, thresholds) = status_contract_snapshot();
    let build = BuildInfo {
        sha: "abc1234".to_string(),
        dirty: Some(false),
        built_at: "2026-01-01T00:00:00Z".to_string(),
    };
    // Real content, not an empty map: `groupColors` must be exercised by
    // the fixture the same way `groups`/`reservedGroups` already are, or a
    // future renderer bug that stops emitting real colors would pass this
    // pin silently. Matches alice's `["codereview", "dev"]` membership
    // above so a reader can see the shape it feeds without cross-checking
    // another test.
    let group_colors: std::collections::BTreeMap<String, String> = [
        (
            "codereview".to_string(),
            config::derive_group_color("codereview"),
        ),
        ("dev".to_string(), "#0a84ff".to_string()),
    ]
    .into_iter()
    .collect();

    // Trailing newline so the file is a well-formed text file; the renderer
    // itself emits none.
    format!(
        "{}\n",
        render_accounts_json(
            &snapshot,
            &thresholds,
            StatusSource::Live,
            Some(&build),
            false,
            None,
            &group_colors,
        )
    )
}

/// A canonical Codex row before the facts its state adds: active, probed `ok`, in no
/// group, nothing served, every Codex fact unreported. Built by [`contract_account`],
/// so a new [`AccountSnapshot`] field breaks this sample as it breaks the contract one.
fn canonical_codex_row(name: &str, priority: i64, plan: &str, org_uuid: &str) -> AccountSnapshot {
    AccountSnapshot {
        provider: Provider::Codex,
        ..contract_account(
            name,
            priority,
            "active",
            false,
            None,
            None,
            None,
            None,
            None,
            None,
            ProbeStatus::Ok,
            None,
            QuotaState::Normal,
            &[],
            None,
            (Some(plan), None, None),
            (Some(org_uuid), None),
        )
    }
}

/// A snapshot holding `accounts` and nothing else: no account serving, no session, no log.
fn snapshot_of(accounts: Vec<AccountSnapshot>) -> StatsSnapshot {
    StatsSnapshot {
        accounts,
        current: None,
        current_by_provider: crate::stats::CurrentByProvider::default(),
        recent: Vec::new(),
        sessions: Vec::new(),
        wire_sessions: Vec::new(),
        wire_sessions_summary: tcr_status_wire::SessionsSummary::default(),
    }
}
