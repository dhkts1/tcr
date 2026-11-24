//! Tests for Codex rows in `tcr status --json` and `tcr accounts` (`cli/render.rs`), checked
//! against the canonical Codex fixtures.

use super::*;
/// One of the canonical Codex fixtures, read from where TcrBar's decode tests read it
/// (`apps/macos/Tests/Fixtures/`). The same file, never a copy: one set of fixtures
/// with two proofs, the panel's decode and this renderer's output, is what keeps the
/// producer and the consumer from drifting apart.
fn canonical_fixture(name: &str) -> serde_json::Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("apps/macos/Tests/Fixtures")
        .join(name);
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read the canonical fixture {}: {e}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("the canonical fixture {} is not JSON: {e}", path.display()))
}

/// The canonical fixtures' clock, 2026-01-01T00:00:00Z. Every time in them is an
/// offset from it.
const CANONICAL_NOW_MS: i64 = 1_767_225_600_000;
const SECOND_MS: i64 = 1_000;
const MINUTE_MS: i64 = 60 * SECOND_MS;
const HOUR_MS: i64 = 60 * MINUTE_MS;
const DAY_MS: i64 = 24 * HOUR_MS;
/// A weekly window's length, 10,080 minutes.
const WEEK_MS: i64 = 7 * DAY_MS;

/// The keys a Codex row may carry and an Anthropic row never does.
const CODEX_ROW_KEYS: [&str; 12] = [
    "quotaWindows",
    "fiveHourWindowMinutes",
    "sevenDayWindowMinutes",
    "limitReachedType",
    "spendControlReached",
    "credits",
    "loginError",
    "codexHome",
    "tokenSyncPending",
    "resetCredits",
    "codexRoute",
    "codexTraffic",
];

/// The instant `offset_ms` from the canonical clock.
fn canonical_at(offset_ms: i64) -> OffsetDateTime {
    fixture_instant(CANONICAL_NOW_MS + offset_ms)
}

/// The serving build both fixtures name.
fn fixture_build() -> BuildInfo {
    BuildInfo {
        sha: "abc1234".to_string(),
        dirty: Some(false),
        built_at: "2026-01-01T00:00:00Z".to_string(),
    }
}

/// The fleet-wide Codex facts every canonical Codex row repeats: codex is pooled, and
/// in the last ten minutes tcr refused twelve requests itself and relayed three on the
/// client's own credential.
fn canonical_codex_route_and_traffic()
-> (tcr_status_wire::CodexRoute, tcr_status_wire::CodexTraffic) {
    (
        tcr_status_wire::CodexRoute {
            pooled: true,
            ..Default::default()
        },
        tcr_status_wire::CodexTraffic {
            refused_10m: 12,
            last_refusal: Some(tcr_status_wire::CodexRefusalKind::UsageLimitReached),
            last_refusal_at_ms: Some(1_767_225_560_000),
            unpooled_10m: 3,
        },
    )
}

/// The keys whose values differ between two JSON objects, from the union of both
/// objects' keys, so a key only one side carries is named too.
fn differing_keys(got: &serde_json::Value, want: &serde_json::Value) -> Vec<String> {
    let mut keys: Vec<String> = got
        .as_object()
        .into_iter()
        .chain(want.as_object())
        .flat_map(|object| object.keys().cloned())
        .collect();
    keys.sort();
    keys.dedup();
    keys.retain(|key| got.get(key) != want.get(key));
    keys
}

/// One bucket of a canonical Codex row's usage, as `Totals::to_wire` reports traffic no
/// configured rate prices, which is all Codex traffic: every request unpriced, so
/// `costUsd` is `null` once the bucket served anything, and the measured zero `0.0`
/// while it served nothing. `input` is BASE input, the row's quota counter less cache
/// reads, as `Totals::to_wire` reports it.
fn codex_bucket(
    requests: u64,
    input: u64,
    cache_read: u64,
    output: u64,
    reasoning: u64,
) -> tcr_status_wire::UsageTotals {
    tcr_status_wire::UsageTotals {
        requests,
        input_tokens: input,
        cache_read_tokens: cache_read,
        output_tokens: output,
        reasoning_tokens: reasoning,
        unpriced_requests: requests,
        cost_usd: if requests == 0 { Some(0.0) } else { None },
        ..Default::default()
    }
}

/// A canonical Codex row's usage. Each window opens where the usage tracker opens it,
/// read off the row's own resets so the two cannot disagree: the five-hour window at
/// the five-hour reset minus five hours, the weekly window at the weekly reset minus a
/// week. `todayByModel` holds today's one model once today served anything.
fn canonical_codex_usage(
    row: &AccountSnapshot,
    today: tcr_status_wire::UsageTotals,
    last_hour: tcr_status_wire::UsageTotals,
    window: Option<tcr_status_wire::UsageTotals>,
    seven_day_window: Option<tcr_status_wire::UsageTotals>,
) -> tcr_status_wire::UsageRow {
    let opened = |reset: Option<OffsetDateTime>, length_ms: i64| {
        let reset = reset.expect("a window's usage needs the window's reset");
        (reset.unix_timestamp_nanos() / 1_000_000) as i64 - length_ms
    };
    tcr_status_wire::UsageRow {
        today,
        window: window.map(|totals| tcr_status_wire::UsageWindow {
            since: opened(row.five_hour_reset, crate::usage::FIVE_HOURS_MS),
            totals,
        }),
        last_hour,
        today_by_model: if today.requests > 0 {
            std::iter::once(("gpt-5.1-codex".to_string(), today)).collect()
        } else {
            std::collections::BTreeMap::new()
        },
        seven_day_window: seven_day_window.map(|totals| tcr_status_wire::UsageWindow {
            since: opened(row.seven_day_reset, WEEK_MS),
            totals,
        }),
    }
}

/// The snapshot behind `codex-status.json`: its ten rows in fixture order, alice's
/// Claude row and one Codex row per state, each with a threshold of 0.85.
///
/// Only facts are set here. Every derived key, `quota`, the per-window states, `held`,
/// `secondsUntilFree`, `cacheHitRatio` and `plan`, is the renderer's to compute, which
/// is what makes the comparison a proof of the renderer rather than of this function.
fn canonical_codex_snapshot() -> (StatsSnapshot, Vec<f64>) {
    use crate::stats::GateReason;
    use tcr_status_wire::CodexCredits;

    let at = canonical_at;
    let idle = codex_bucket(0, 0, 0, 0, 0);
    let weekly_only = || CodexFacts {
        quota_windows: Some(vec!["sevenDay".to_string()]),
        seven_day_window_minutes: Some(10_080),
        ..CodexFacts::default()
    };
    let both_windows = || CodexFacts {
        quota_windows: Some(vec!["fiveHour".to_string(), "sevenDay".to_string()]),
        five_hour_window_minutes: Some(300),
        seven_day_window_minutes: Some(10_080),
        ..CodexFacts::default()
    };

    // Claude, OK, in ORCHARD: contract row 1 with its windows moved into the future.
    let mut alice_usage = contract_usage("claude-sonnet-5");
    alice_usage.window = alice_usage.window.map(|w| tcr_status_wire::UsageWindow {
        since: 1_767_217_140_000,
        ..w
    });
    let alice = contract_account(
        "alice@example.com",
        0,
        "active",
        false,
        Some(0.47),
        Some(at(2 * HOUR_MS + 39 * MINUTE_MS)),
        Some(0.20),
        Some(at(4 * DAY_MS)),
        Some(0.62),
        Some(at(4 * DAY_MS)),
        ProbeStatus::Ok,
        None,
        QuotaState::Normal,
        &["orchard"],
        Some(alice_usage),
        (Some("claude_max"), Some("default_claude_max_20x"), None),
        (
            Some("11111111-1111-1111-1111-111111111111"),
            Some("Example Personal"),
        ),
    );

    // OK, one weekly window.
    let mut bob = AccountSnapshot {
        seven_day: Some(0.34),
        seven_day_reset: Some(at(4 * DAY_MS)),
        last_probe: Some(at(-30 * SECOND_MS)),
        requests: 42,
        input_tokens: 1_100_000,
        cache_read_tokens: 902_000,
        output_tokens: 22_000,
        codex: weekly_only(),
        ..canonical_codex_row(
            "bob@example.com/codex",
            1,
            "pro",
            "55555555-5555-4555-8555-555555555555",
        )
    };
    bob.usage = Some(canonical_codex_usage(
        &bob,
        codex_bucket(42, 198_000, 902_000, 22_000, 9_000),
        codex_bucket(7, 30_000, 160_000, 4_100, 1_200),
        None,
        Some(codex_bucket(
            1_480, 11_000_000, 50_000_000, 1_100_000, 420_000,
        )),
    ));

    // SPENT on its weekly window, with credits left.
    let mut carol = AccountSnapshot {
        seven_day: Some(1.0),
        seven_day_reset: Some(at(3 * DAY_MS + 4 * HOUR_MS)),
        last_probe: Some(at(-30 * SECOND_MS)),
        quota_state: QuotaState::Exhausted,
        gate: GateReason::SevenDay,
        free_at: Some(at(3 * DAY_MS + 4 * HOUR_MS)),
        requests: 310,
        input_tokens: 5_400_000,
        cache_read_tokens: 4_806_000,
        output_tokens: 61_000,
        codex: CodexFacts {
            limit_reached_type: Some("rate_limit_reached".to_string()),
            credits: Some(CodexCredits {
                has_credits: true,
                unlimited: false,
                balance: Some("12.50".to_string()),
            }),
            ..weekly_only()
        },
        ..canonical_codex_row(
            "carol@example.com",
            2,
            "pro",
            "66666666-6666-4666-8666-666666666666",
        )
    };
    carol.usage = Some(canonical_codex_usage(
        &carol,
        codex_bucket(310, 594_000, 4_806_000, 61_000, 21_000),
        idle,
        None,
        Some(codex_bucket(
            2_900,
            20_000_000,
            140_000_000,
            5_400_000,
            2_100_000,
        )),
    ));

    // OK, both windows.
    let mut dana = AccountSnapshot {
        five_hour: Some(0.18),
        five_hour_reset: Some(at(3 * HOUR_MS + 2 * MINUTE_MS)),
        seven_day: Some(0.25),
        seven_day_reset: Some(at(5 * DAY_MS)),
        last_probe: Some(at(-30 * SECOND_MS)),
        requests: 96,
        input_tokens: 2_000_000,
        cache_read_tokens: 1_700_000,
        output_tokens: 260_000,
        codex: both_windows(),
        ..canonical_codex_row(
            "dana@example.com",
            3,
            "plus",
            "77777777-7777-4777-8777-777777777777",
        )
    };
    dana.usage = Some(canonical_codex_usage(
        &dana,
        codex_bucket(96, 300_000, 1_700_000, 260_000, 70_000),
        codex_bucket(9, 30_000, 150_000, 24_000, 6_000),
        Some(codex_bucket(31, 100_000, 540_000, 210_000, 55_000)),
        Some(codex_bucket(400, 1_400_000, 7_600_000, 640_000, 180_000)),
    ));

    // NEEDS RE-LOGIN, imported from a folder.
    let mut erin = AccountSnapshot {
        status: "error".to_string(),
        five_hour: Some(0.0),
        five_hour_reset: None,
        seven_day: Some(0.40),
        seven_day_reset: Some(at(2 * DAY_MS)),
        probe_status: ProbeStatus::Error,
        last_probe: Some(at(-MINUTE_MS)),
        probe_error: Some("refresh_token_reused".to_string()),
        gate: GateReason::Login,
        codex: CodexFacts {
            login_error: Some("refresh_token_reused".to_string()),
            codex_home: Some("/Users/example/.codex".to_string()),
            ..both_windows()
        },
        ..canonical_codex_row(
            "erin@example.com/codex",
            4,
            "team",
            "88888888-8888-4888-8888-888888888888",
        )
    };
    erin.usage = Some(canonical_codex_usage(
        &erin,
        idle,
        idle,
        None,
        Some(codex_bucket(120, 1_000_000, 4_000_000, 140_000, 50_000)),
    ));

    // REFUSED with no window full: the spend control.
    let mut gil = AccountSnapshot {
        five_hour: Some(0.15),
        five_hour_reset: Some(at(3 * HOUR_MS + 2 * MINUTE_MS)),
        seven_day: Some(0.10),
        seven_day_reset: Some(at(6 * DAY_MS)),
        last_probe: Some(at(-MINUTE_MS)),
        gate: GateReason::Rejected,
        requests: 24,
        input_tokens: 400_000,
        cache_read_tokens: 284_000,
        output_tokens: 52_000,
        codex: CodexFacts {
            limit_reached_type: Some("rate_limit_reached".to_string()),
            spend_control_reached: true,
            ..both_windows()
        },
        ..canonical_codex_row(
            "gil@example.com",
            5,
            "team",
            "99999999-9999-4999-8999-999999999999",
        )
    };
    gil.usage = Some(canonical_codex_usage(
        &gil,
        codex_bucket(24, 116_000, 284_000, 52_000, 14_000),
        codex_bucket(2, 9_000, 21_000, 3_000, 800),
        Some(codex_bucket(20, 96_000, 234_000, 48_000, 13_000)),
        Some(codex_bucket(140, 900_000, 2_100_000, 310_000, 90_000)),
    ));

    // REFUSED, still held: throttled for the 47 minutes its hold has left, since a Codex
    // hold never outlasts an hour.
    let mut hal = AccountSnapshot {
        status: "throttled".to_string(),
        seven_day: Some(0.60),
        seven_day_reset: Some(at(5 * DAY_MS)),
        last_probe: Some(at(-2 * MINUTE_MS)),
        gate: GateReason::Hold,
        free_at: Some(at(47 * MINUTE_MS)),
        rate_limited_until: Some(at(47 * MINUTE_MS)),
        requests: 77,
        input_tokens: 1_900_000,
        cache_read_tokens: 1_520_000,
        output_tokens: 40_000,
        codex: CodexFacts {
            limit_reached_type: Some("rate_limit_reached".to_string()),
            ..weekly_only()
        },
        ..canonical_codex_row(
            "hal@example.com",
            6,
            "pro",
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
        )
    };
    hal.usage = Some(canonical_codex_usage(
        &hal,
        codex_bucket(77, 380_000, 1_520_000, 40_000, 15_000),
        idle,
        None,
        Some(codex_bucket(900, 8_000_000, 32_000_000, 1_900_000, 700_000)),
    ));

    // NEAR on its five-hour window, in ORCHARD.
    let mut ivy = AccountSnapshot {
        five_hour: Some(0.88),
        five_hour_reset: Some(at(52 * MINUTE_MS)),
        seven_day: Some(0.17),
        seven_day_reset: Some(at(6 * DAY_MS)),
        last_probe: Some(at(-40 * SECOND_MS)),
        quota_state: QuotaState::NearLimit,
        gate: GateReason::FiveHour,
        free_at: Some(at(52 * MINUTE_MS)),
        groups: vec!["orchard".to_string()],
        requests: 60,
        input_tokens: 1_300_000,
        cache_read_tokens: 1_014_000,
        output_tokens: 260_000,
        codex: both_windows(),
        ..canonical_codex_row(
            "ivy@example.com",
            7,
            "plus",
            "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
        )
    };
    ivy.usage = Some(canonical_codex_usage(
        &ivy,
        codex_bucket(60, 286_000, 1_014_000, 260_000, 70_000),
        codex_bucket(20, 90_000, 330_000, 80_000, 22_000),
        Some(codex_bucket(52, 240_000, 860_000, 210_000, 60_000)),
        Some(codex_bucket(300, 1_600_000, 5_400_000, 900_000, 250_000)),
    ));

    // OK with no usage window at all, and unlimited credits.
    let mut jo = AccountSnapshot {
        last_probe: Some(at(-50 * SECOND_MS)),
        requests: 30,
        input_tokens: 300_000,
        cache_read_tokens: 225_000,
        output_tokens: 6_000,
        codex: CodexFacts {
            quota_windows: Some(Vec::new()),
            credits: Some(CodexCredits {
                has_credits: true,
                unlimited: true,
                balance: None,
            }),
            ..CodexFacts::default()
        },
        ..canonical_codex_row(
            "jo@example.com",
            8,
            "business",
            "cccccccc-cccc-4ccc-8ccc-cccccccccccc",
        )
    };
    jo.usage = Some(canonical_codex_usage(
        &jo,
        codex_bucket(30, 75_000, 225_000, 6_000, 1_000),
        idle,
        None,
        None,
    ));

    // UNREACHABLE: its one group, RESEARCH, is reserved.
    let mut kim = AccountSnapshot {
        seven_day: Some(0.05),
        seven_day_reset: Some(at(6 * DAY_MS)),
        last_probe: Some(at(-3 * MINUTE_MS)),
        gate: GateReason::Reserved,
        groups: vec!["research".to_string()],
        reserved_groups: vec!["research".to_string()],
        codex: weekly_only(),
        ..canonical_codex_row(
            "kim@example.com",
            9,
            "pro",
            "dddddddd-dddd-4ddd-8ddd-dddddddddddd",
        )
    };
    kim.usage = Some(canonical_codex_usage(
        &kim,
        idle,
        idle,
        None,
        Some(codex_bucket(40, 200_000, 700_000, 30_000, 9_000)),
    ));

    let accounts = vec![alice, bob, carol, dana, erin, gil, hal, ivy, jo, kim];
    let thresholds = vec![0.85; accounts.len()];
    (
        StatsSnapshot {
            accounts,
            current: None,
            current_by_provider: crate::stats::CurrentByProvider::default(),
            recent: Vec::new(),
            sessions: Vec::new(),
            wire_sessions: Vec::new(),
            wire_sessions_summary: tcr_status_wire::SessionsSummary::default(),
        },
        thresholds,
    )
}

/// The two sessions behind `codex-sessions.json`: a Claude session with a running
/// `Bash`, and a Codex session with a running `exec_command`. That is codex's default,
/// interactive tool, which has no timeout argument, so the call carries none.
fn canonical_codex_sessions() -> Vec<tcr_status_wire::SessionRow> {
    let ms = |offset_ms: i64| CANONICAL_NOW_MS + offset_ms;
    let ramp = |zeros: usize, tail: &[u16]| -> Vec<u16> {
        std::iter::repeat_n(0, zeros)
            .chain(tail.iter().copied())
            .collect()
    };
    let bucket = |tool: &str, calls: u64, seconds_p50: f64| tcr_status_wire::ToolBucketRow {
        tool: tool.to_string(),
        calls,
        seconds_p50,
        ..Default::default()
    };
    let slow = |tool: &str, seconds: f64, head: &str, class: Option<&str>, ended_ms: i64| {
        tcr_status_wire::SlowToolRow {
            tool: tool.to_string(),
            seconds,
            command_head: Some(head.to_string()),
            command_class: class.map(str::to_string),
            ended_ms,
        }
    };
    let claude = tcr_status_wire::SessionRow {
        session_id: "a1f3c2d4-0000-4000-8000-000000000001".to_string(),
        account: Some("alice@example.com".to_string()),
        model: Some("claude-opus-5".to_string()),
        first_seen_ms: ms(-40 * MINUTE_MS),
        last_seen_ms: ms(-3 * SECOND_MS),
        requests: 24,
        input_tokens: 180_000,
        output_tokens: 9_200,
        cache_read_tokens: 160_000,
        cache_creation_tokens: 12_000,
        tools: tcr_status_wire::SessionToolsRow {
            calls: 12,
            running: vec![tcr_status_wire::RunningToolRow {
                tool: "Bash".to_string(),
                started_ms: ms(-20 * SECOND_MS),
                command_head: Some("cargo test".to_string()),
                command_class: Some("build".to_string()),
                timeout_seconds: None,
            }],
            by_tool: vec![bucket("Bash", 8, 2.5), bucket("Read", 4, 0.1)],
            // The ten slowest of its twelve calls.
            slowest: vec![
                slow(
                    "Bash",
                    12.0,
                    "cargo build --release",
                    Some("build"),
                    ms(-295 * SECOND_MS),
                ),
                slow(
                    "Bash",
                    6.0,
                    "cargo build",
                    Some("build"),
                    ms(-230 * SECOND_MS),
                ),
                slow(
                    "Bash",
                    3.0,
                    "rg -n TODO",
                    Some("search"),
                    ms(-175 * SECOND_MS),
                ),
                slow(
                    "Bash",
                    2.5,
                    "cargo test -p example",
                    Some("build"),
                    ms(-250 * SECOND_MS),
                ),
                slow(
                    "Bash",
                    2.5,
                    "git status",
                    Some("git-local"),
                    ms(-160 * SECOND_MS),
                ),
                slow(
                    "Bash",
                    2.0,
                    "git status",
                    Some("git-local"),
                    ms(-320 * SECOND_MS),
                ),
                slow(
                    "Bash",
                    2.0,
                    "cargo test",
                    Some("build"),
                    ms(-110 * SECOND_MS),
                ),
                slow(
                    "Bash",
                    1.5,
                    "git status",
                    Some("git-local"),
                    ms(-125 * SECOND_MS),
                ),
                slow(
                    "Read",
                    0.4,
                    "Read /Users/example/git/tcr/src/session_wire.rs",
                    None,
                    ms(-280 * SECOND_MS),
                ),
                slow(
                    "Read",
                    0.1,
                    "Read /Users/example/git/tcr/src/proxy.rs",
                    None,
                    ms(-350 * SECOND_MS),
                ),
            ],
            ..Default::default()
        },
        // Twenty of its 24 requests fall in the ring; the other four came before it.
        req_per_minute: ramp(22, &[1, 2, 3, 2, 4, 3, 5, 0]),
        cost_usd: 1.2034,
        cache_ttl_secs: Some(3600),
        provider: None,
        unpriced_requests: 0,
        reasoning_tokens: 0,
    };
    let codex = tcr_status_wire::SessionRow {
        session_id: "019a3c5e-0000-7000-8000-000000000002".to_string(),
        account: Some("bob@example.com/codex".to_string()),
        model: Some("gpt-5.1-codex".to_string()),
        first_seen_ms: ms(-175 * SECOND_MS),
        last_seen_ms: ms(-50 * SECOND_MS),
        requests: 7,
        input_tokens: 95_000,
        output_tokens: 4_100,
        cache_read_tokens: 80_000,
        cache_creation_tokens: 0,
        tools: tcr_status_wire::SessionToolsRow {
            calls: 3,
            running: vec![tcr_status_wire::RunningToolRow {
                tool: "exec_command".to_string(),
                started_ms: ms(-45 * SECOND_MS),
                command_head: Some("cargo build".to_string()),
                command_class: Some("build".to_string()),
                timeout_seconds: None,
            }],
            by_tool: vec![
                bucket("exec_command", 2, 4.0),
                bucket("apply_patch", 1, 0.2),
            ],
            slowest: vec![
                slow(
                    "exec_command",
                    4.0,
                    "cargo test",
                    Some("build"),
                    ms(-115 * SECOND_MS),
                ),
                slow(
                    "exec_command",
                    4.0,
                    "cargo check",
                    Some("build"),
                    ms(-55_500),
                ),
                slow("apply_patch", 0.2, "apply_patch", None, ms(-94_800)),
            ],
            ..Default::default()
        },
        req_per_minute: ramp(26, &[2, 3, 2, 0]),
        cost_usd: 0.0,
        cache_ttl_secs: None,
        provider: Some("codex".to_string()),
        unpriced_requests: 7,
        reasoning_tokens: 1_200,
    };
    vec![claude, codex]
}

/// `status_rows_json(account_status_rows_at(..))`, given the facts behind `codex-status.json`, emits exactly
/// the rows TcrBar decodes from that file: every key and every derived value, compared
/// as `serde_json::Value`s with no normalisation and no rounding.
///
/// The fixture belongs to the panel, whose decode tests read the same file; this is
/// the producer's half of that contract. A key renamed, dropped or computed differently
/// fails here before it reaches a screen. The render runs at the fixture's own clock,
/// so `held` and `secondsUntilFree` come out as committed with nothing to normalise.
#[test]
fn canonical_codex_status_matches_the_renderer() {
    let (snapshot, thresholds) = canonical_codex_snapshot();
    let (route, traffic) = canonical_codex_route_and_traffic();
    let group_colors: std::collections::BTreeMap<String, String> = [
        ("orchard".to_string(), "#30d158".to_string()),
        ("research".to_string(), "#bf5af2".to_string()),
    ]
    .into_iter()
    .collect();
    let rendered = status_rows_json(account_status_rows_at(
        fixture_instant(CANONICAL_NOW_MS),
        &snapshot,
        &thresholds,
        StatusSource::Live,
        Some(&fixture_build()),
        false,
        None,
        &group_colors,
        Some(&route),
        Some(&traffic),
    ));
    let rendered: Vec<serde_json::Value> =
        serde_json::from_str(&rendered).expect("the renderer emits a JSON array");
    let serde_json::Value::Array(fixture) = canonical_fixture("codex-status.json") else {
        panic!("codex-status.json is a bare array");
    };

    assert_eq!(fixture.len(), 10, "codex-status.json carries ten rows");
    assert_eq!(rendered.len(), 10, "the renderer emits one row per account");
    let names = |rows: &[serde_json::Value]| -> Vec<String> {
        rows.iter()
            .map(|r| r["name"].as_str().unwrap_or_default().to_string())
            .collect()
    };
    assert_eq!(
        names(&rendered),
        names(&fixture),
        "the renderer emits the fixture's rows in the fixture's order"
    );
    // Row by row, naming the keys: one assertion over the whole array prints over a
    // thousand lines and points at none of them.
    for (got, want) in rendered.iter().zip(&fixture) {
        assert!(
            got == want,
            "{} differs from the canonical fixture on {:?}\nrendered: {got:#}\nfixture: {want:#}",
            want["name"],
            differing_keys(got, want)
        );
    }
}

/// `render_sessions_json` emits exactly `codex-sessions.json` for the two sessions
/// behind it.
///
/// The session shape the panel decodes, pinned the way the status rows are: the Codex
/// keys present on the Codex session, and absent, never `null` or `0`, on the Claude one,
/// whose bytes are the ones it had before Codex existed.
#[test]
fn canonical_codex_sessions_match_the_renderer() {
    let rendered: serde_json::Value = serde_json::from_str(&render_sessions_json(
        true,
        &canonical_codex_sessions(),
        None,
        None,
    ))
    .expect("valid JSON");
    let fixture = canonical_fixture("codex-sessions.json");
    let (Some(got), Some(want)) = (
        rendered["sessions"].as_array(),
        fixture["sessions"].as_array(),
    ) else {
        panic!("both documents carry a sessions array: {rendered} / {fixture}");
    };
    for (got, want) in got.iter().zip(want) {
        assert!(
            got == want,
            "session {} differs from the canonical fixture on {:?}\nrendered: {got:#}\nfixture: {want:#}",
            want["sessionId"],
            differing_keys(got, want)
        );
    }
    assert_eq!(
        rendered, fixture,
        "render_sessions_json no longer emits codex-sessions.json"
    );
}

/// **A Codex row's held window is labelled by its length**, in the JSON `held` array and in
/// the text `held=` token alike: a 30-day weekly window reads `30d`, where the fixed label
/// would say `7d`, the length the plan does not have. Without a reported length the label
/// is the fixed one, and an Anthropic row keeps `7d` whatever its facts hold.
///
/// Watched red: label every window by its slot, as before, and the 30-day row reads `7d`.
#[test]
fn a_codex_held_window_is_labelled_by_its_length() {
    let now = fixture_instant(CANONICAL_NOW_MS);
    let full_weekly = |provider: Provider, minutes: Option<u32>| AccountSnapshot {
        provider,
        seven_day: Some(1.0),
        seven_day_reset: Some(canonical_at(3 * DAY_MS)),
        codex: CodexFacts {
            seven_day_window_minutes: minutes,
            ..CodexFacts::default()
        },
        ..canonical_codex_row(
            "bob@example.com/codex",
            0,
            "pro",
            "55555555-5555-4555-8555-555555555555",
        )
    };
    let held_labels = |row: AccountSnapshot| -> (Vec<String>, String) {
        let snapshot = StatsSnapshot {
            accounts: vec![row.clone()],
            current: None,
            current_by_provider: crate::stats::CurrentByProvider::default(),
            recent: Vec::new(),
            sessions: Vec::new(),
            wire_sessions: Vec::new(),
            wire_sessions_summary: tcr_status_wire::SessionsSummary::default(),
        };
        let json = status_rows_json(account_status_rows_at(
            now,
            &snapshot,
            &[0.85],
            StatusSource::Live,
            None,
            false,
            None,
            &std::collections::BTreeMap::new(),
            None,
            None,
        ));
        let rows: Vec<serde_json::Value> =
            serde_json::from_str(&json).expect("the renderer emits a JSON array");
        let held = rows[0]["held"]
            .as_array()
            .expect("every row carries held")
            .iter()
            .map(|h| h["window"].as_str().unwrap_or_default().to_string())
            .collect();
        (held, held_suffix(&row, 0.85, now))
    };

    let (json, text) = held_labels(full_weekly(Provider::Codex, Some(43_200)));
    assert_eq!(
        json,
        vec!["30d".to_string()],
        "a 30-day window, on the wire"
    );
    assert!(
        text.starts_with(" held=30d "),
        "and in the text token: {text}"
    );

    let (json, text) = held_labels(full_weekly(Provider::Codex, None));
    assert_eq!(
        json,
        vec!["7d".to_string()],
        "no reported length: the fixed label"
    );
    assert!(text.starts_with(" held=7d "), "{text}");

    let (json, text) = held_labels(full_weekly(Provider::Anthropic, Some(43_200)));
    assert_eq!(
        json,
        vec!["7d".to_string()],
        "an Anthropic row is unchanged"
    );
    assert!(text.starts_with(" held=7d "), "{text}");
}

/// An Anthropic row renders exactly as the committed contract fixture holds it, with no
/// Codex key: with no Codex fact, and again with every Codex fact set and a Codex route
/// and traffic passed in.
///
/// This is the compatibility promise for a Claude-only fleet: `tcr status --json` prints
/// the bytes it always did. The renderer writes the Codex keys on Codex rows only, so
/// neither a snapshot that wrongly carries a Codex fact nor a fleet-wide route can reach
/// an Anthropic row. Compared with the committed file, never a regenerated copy.
#[test]
fn anthropic_rows_serialize_with_no_new_key() {
    let (mut snapshot, mut thresholds) = status_contract_snapshot();
    snapshot.accounts.truncate(4);
    thresholds.truncate(4);
    assert!(
        snapshot
            .accounts
            .iter()
            .all(|a| a.provider == Provider::Anthropic),
        "the contract's first four rows are its Anthropic rows"
    );
    assert_eq!(thresholds, vec![0.9; 4]);

    let committed: Vec<serde_json::Value> = serde_json::from_str(
        &fs::read_to_string(status_contract_fixture_path())
            .expect("committed contract fixture is readable"),
    )
    .expect("the contract fixture is a bare JSON array");
    let group_colors: std::collections::BTreeMap<String, String> = [
        (
            "codereview".to_string(),
            config::derive_group_color("codereview"),
        ),
        ("dev".to_string(), "#0a84ff".to_string()),
    ]
    .into_iter()
    .collect();
    let (route, traffic) = canonical_codex_route_and_traffic();
    let render = |snapshot: &StatsSnapshot| -> Vec<serde_json::Value> {
        let json = status_rows_json(account_status_rows_at(
            // After every instant in the contract sample, so each clock-derived field
            // clamps exactly as it does in the committed file.
            fixture_instant(1_767_398_400_000),
            snapshot,
            &thresholds,
            StatusSource::Live,
            Some(&fixture_build()),
            false,
            None,
            &group_colors,
            Some(&route),
            Some(&traffic),
        ));
        serde_json::from_str(&json).expect("the renderer emits a JSON array")
    };
    let check = |pass: &str, rows: Vec<serde_json::Value>| {
        assert_eq!(rows.len(), 4, "{pass}: one row per account");
        for (row, committed) in rows.iter().zip(&committed) {
            let name = row["name"].as_str().expect("every row has a name");
            for key in CODEX_ROW_KEYS {
                assert!(row.get(key).is_none(), "{pass}: {name} carries {key}");
            }
            if let Some(usage) = row.get("usage") {
                assert!(
                    usage.get("sevenDayWindow").is_none(),
                    "{pass}: {name}'s usage carries sevenDayWindow"
                );
                let mut buckets = vec![&usage["today"], &usage["lastHour"]];
                buckets.extend(usage.get("window"));
                buckets.extend(
                    usage["todayByModel"]
                        .as_object()
                        .expect("todayByModel is an object")
                        .values(),
                );
                for bucket in buckets {
                    assert!(
                        bucket.get("reasoningTokens").is_none(),
                        "{pass}: a bucket of {name}'s usage carries reasoningTokens: {bucket}"
                    );
                }
            }
            assert_eq!(
                row, committed,
                "{pass}: {name} is not the committed contract row"
            );
        }
    };

    check("with no Codex fact", render(&snapshot));

    for account in &mut snapshot.accounts {
        account.codex = CodexFacts {
            quota_windows: Some(vec!["fiveHour".to_string(), "sevenDay".to_string()]),
            five_hour_window_minutes: Some(300),
            seven_day_window_minutes: Some(10_080),
            limit_reached_type: Some("rate_limit_reached".to_string()),
            spend_control_reached: true,
            credits: Some(tcr_status_wire::CodexCredits {
                has_credits: true,
                unlimited: false,
                balance: Some("12.50".to_string()),
            }),
            login_error: Some("refresh_token_reused".to_string()),
            codex_home: Some("/Users/example/.codex".to_string()),
            token_sync_pending: true,
            banked_resets: None,
            auto_reset: None,
            reset_credits: Some(tcr_status_wire::ResetCredits {
                available_count: 3,
                earliest_expiry_ms: Some(1_800_000_000_000),
            }),
        };
    }
    check("with every Codex fact set", render(&snapshot));
}

/// A Codex row says it is one right after its name, and ends with what only a Codex row can
/// say, after `parked_groups=` and in a fixed order, each token only when it has something
/// to say. A Claude row never prints any of it, whatever its Codex facts hold.
#[test]
fn codex_account_line_tokens_follow_parked_groups_in_order() {
    use crate::stats::GateReason;
    use tcr_status_wire::CodexCredits;

    let mut kim = canonical_codex_row(
        "kim@example.com",
        9,
        "pro",
        "dddddddd-dddd-4ddd-8ddd-dddddddddddd",
    );
    kim.groups = vec!["orchard".to_string(), "research".to_string()];
    kim.reserved_groups = vec!["research".to_string()];
    kim.parked_groups = vec!["orchard".to_string()];
    kim.gate = GateReason::Reserved;
    kim.codex.login_error = Some("refresh_token_reused".to_string());
    kim.codex.limit_reached_type = Some("rate_limit_reached".to_string());
    kim.codex.spend_control_reached = true;
    kim.codex.credits = Some(CodexCredits {
        has_credits: true,
        unlimited: false,
        balance: Some("12.50".to_string()),
    });
    let line = |row: &AccountSnapshot| {
        render_accounts(&snapshot_of(vec![row.clone()]), StatusSource::Live)
    };

    assert_eq!(
        line(&kim),
        "account kim@example.com provider=codex priority=9 plan=ChatGPT Pro 5h=n/a wk=n/a \
             cache=n/a state=ok status=active probe=ok stream_errors=0 \
             groups=orchard,research reserved_groups=research parked_groups=orchard \
             login_error=refresh_token_reused limit_reached=rate_limit_reached \
             spend_control=reached credits=12.50 reachable=no workspace=dddddddd\n"
    );

    let no_workspace = AccountSnapshot {
        org_uuid: None,
        ..kim.clone()
    };
    assert_eq!(
        line(&no_workspace),
        line(&kim).replace(" workspace=dddddddd", ""),
        "no workspace id, no workspace token"
    );

    let mut unlimited = kim.clone();
    unlimited.codex.credits = Some(CodexCredits {
        has_credits: false,
        unlimited: true,
        balance: None,
    });
    assert_eq!(
        line(&unlimited),
        line(&kim).replace(" credits=12.50", " credits=unlimited")
    );

    let mut spent = kim.clone();
    spent.codex.credits = Some(CodexCredits {
        has_credits: false,
        unlimited: false,
        balance: Some("0.00".to_string()),
    });
    assert_eq!(
        line(&spent),
        line(&kim).replace(" credits=12.50", ""),
        "a balance with no credits behind it says nothing"
    );

    let reachable = AccountSnapshot {
        gate: GateReason::Ok,
        ..kim.clone()
    };
    assert_eq!(
        line(&reachable),
        line(&kim).replace(" reachable=no", ""),
        "only a reserved Codex row is unreachable"
    );

    let claude = AccountSnapshot {
        provider: Provider::Anthropic,
        ..kim
    };
    let claude_without_codex_facts = AccountSnapshot {
        codex: CodexFacts::default(),
        ..claude.clone()
    };
    assert_eq!(
        line(&claude),
        line(&claude_without_codex_facts),
        "a Claude row's line never reads its Codex facts"
    );
    for token in [
        " provider=",
        "=none",
        " login_error=",
        " limit_reached=",
        " spend_control=",
        " credits=",
        " reachable=",
        " workspace=",
    ] {
        assert!(
            !line(&claude).contains(token),
            "a Claude row printed {token:?}: {}",
            line(&claude)
        );
    }
}

/// `none` is a window the plan does not report, from the row's own list of windows, and
/// `n/a` keeps meaning a window expected and not yet measured: an unknown list, or a listed
/// window with no reading. A reading left over for a window the plan dropped does not bring
/// it back, and a Claude row never reads the list.
#[test]
fn five_hour_none_and_weekly_none_on_codex_rows() {
    let windows = |list: &[&str]| Some(list.iter().map(ToString::to_string).collect());
    let codex = |name: &str, quota_windows: Option<Vec<String>>| {
        let mut row = canonical_codex_row(name, 0, "pro", "55555555-5555-4555-8555-555555555555");
        row.codex.quota_windows = quota_windows;
        row
    };
    let mut ivy = codex("ivy@example.com", windows(&["sevenDay"]));
    ivy.five_hour = Some(0.5);
    let mut alice = contract_account(
        "alice@example.com",
        0,
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
        (Some("claude_max"), Some("default_claude_max_20x"), None),
        (None, None),
    );
    alice.codex.quota_windows = windows(&[]);
    let rows = vec![
        codex("bob@example.com/codex", windows(&["sevenDay"])),
        codex("jo@example.com", windows(&[])),
        codex("dana@example.com", windows(&["fiveHour", "sevenDay"])),
        codex("hal@example.com", None),
        ivy,
        alice,
    ];
    let text = render_accounts(&snapshot_of(rows), StatusSource::Live);
    let lines: Vec<&str> = text.lines().collect();
    let expected = [
        ("bob@example.com/codex", " 5h=none wk=n/a cache="),
        ("jo@example.com", " 5h=none wk=none cache="),
        ("dana@example.com", " 5h=n/a wk=n/a cache="),
        ("hal@example.com", " 5h=n/a wk=n/a cache="),
        ("ivy@example.com", " 5h=none "),
        ("alice@example.com", " 5h=n/a wk=n/a cache="),
    ];
    assert_eq!(lines.len(), expected.len(), "{text}");
    for (line, (name, holds)) in lines.iter().zip(expected) {
        assert!(
            line.starts_with(&format!("account {name} ")),
            "rows keep their order: {line}"
        );
        assert!(line.contains(holds), "{name} must hold {holds:?}: {line}");
    }
}

/// An Ultra row as measured 2026-11-01: a Max-20x org whose profile names its plan
/// "Ultra" and whose usage read said `"five_hour": null`. Through the server-to-CLI
/// wire (the live `tcr status` path), its line says `plan=Ultra` and `5h=none`, while
/// `5h=n/a` stays for a 5h window that is merely unknown.
#[test]
fn an_ultra_row_reads_plan_ultra_and_five_hour_none_through_the_wire() {
    let row = |name: &str| {
        contract_account(
            name,
            0,
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
            (Some("claude_max"), Some("default_claude_max_20x"), None),
            (None, None),
        )
    };
    let ultra = AccountSnapshot {
        plan_display_name: Some("Ultra".to_string()),
        five_hour_absent_on_plan: true,
        ..row("alice@example.com")
    };
    let unknown = row("bob@example.com");
    let snapshot = snapshot_of(vec![ultra, unknown]);
    let payload =
        StatusPayload::from_snapshot(&snapshot, &[0.95, 0.95], false, None, Default::default());
    let wire = serde_json::to_string(&payload).expect("serialize");
    let back: StatusPayload = serde_json::from_str(&wire).expect("deserialize");
    let (rebuilt, _) = back.into_snapshot();
    let text = render_accounts(&rebuilt, StatusSource::Live);
    let line_for = |name: &str| {
        text.lines()
            .find(|l| l.contains(name))
            .unwrap_or_else(|| panic!("{name} line in {text}"))
            .to_string()
    };
    let alice = line_for("alice@example.com");
    assert!(
        alice.contains(" plan=Ultra 5h=none wk=n/a "),
        "the provider's plan name and the plan's missing 5h window: {alice}"
    );
    let bob = line_for("bob@example.com");
    assert!(
        bob.contains(" plan=Max 20x 5h=n/a "),
        "no display name and no null: today's label and `n/a`: {bob}"
    );
}

/// `tcr status --json` tells a panel which windows a Claude plan has the way it tells it
/// for a Codex plan: an Ultra row (no 5h window) carries `quotaWindows: ["sevenDay"]`,
/// so TcrBar drops its 5h meter instead of drawing it as unmeasured. A Claude row
/// without the flag carries no `quotaWindows` key at all, the bytes it always was.
#[test]
fn status_json_lists_only_the_week_for_a_claude_plan_with_no_five_hour_window() {
    let row = |name: &str| {
        contract_account(
            name,
            0,
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
            (Some("claude_max"), Some("default_claude_max_20x"), None),
            (None, None),
        )
    };
    let ultra = AccountSnapshot {
        plan_display_name: Some("Ultra".to_string()),
        five_hour_absent_on_plan: true,
        ..row("alice@example.com")
    };
    let snapshot = snapshot_of(vec![ultra, row("bob@example.com")]);
    let json = status_json(&snapshot, &[0.95, 0.95], StatusSource::Live);
    let alice = status_row(&json, "alice@example.com");
    assert_eq!(
        alice["quotaWindows"],
        serde_json::json!(["sevenDay"]),
        "a plan with no 5h window lists only the week: {alice}"
    );
    assert!(
        alice.get("provider").is_none(),
        "still an Anthropic row: {alice}"
    );
    let bob = status_row(&json, "bob@example.com");
    assert!(
        bob.get("quotaWindows").is_none(),
        "an unflagged Claude row carries no quotaWindows key: {bob}"
    );
}

/// `tcr accounts` prints `tcr status`'s account line, the Codex tokens included, from its
/// offline snapshot. The Claude row is the line it always was: its workspace id prints no
/// `workspace=`.
#[test]
fn accounts_text_carries_the_codex_tokens() {
    let alice = contract_account(
        "alice@example.com",
        0,
        "active",
        false,
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
        None,
        (Some("claude_max"), Some("default_claude_max_20x"), None),
        (Some("11111111-1111-1111-1111-111111111111"), None),
    );
    let bob = AccountSnapshot {
        probe_status: ProbeStatus::Never,
        ..canonical_codex_row(
            "bob@example.com/codex",
            1,
            "pro",
            "55555555-5555-4555-8555-555555555555",
        )
    };
    assert_eq!(
        render_accounts(&snapshot_of(vec![alice, bob]), StatusSource::Offline),
        "account alice@example.com priority=0 plan=Max 20x 5h=n/a wk=n/a cache=n/a state=ok \
             status=active probe=never stream_errors=n/a\n\
             account bob@example.com/codex provider=codex priority=1 plan=ChatGPT Pro 5h=n/a \
             wk=n/a cache=n/a state=ok status=active probe=never stream_errors=n/a \
             workspace=55555555\n"
    );
}
