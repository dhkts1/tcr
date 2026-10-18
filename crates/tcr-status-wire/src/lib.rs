//! Serde types for the `tcr status --json` wire contract.
//!
//! This is the type that `render_accounts_json` (`src/cli.rs` in the main crate)
//! builds and serializes, and that the macOS app's `FleetStatus.swift` decodes.
//! Before this crate existed, the Rust side had no type at all for this shape —
//! only an ad-hoc `serde_json::json!` literal — so the two sides could drift
//! with nothing to catch it beyond the committed fixture
//! (`tests/fixtures/status-contract.json`) that both sides read.
//!
//! Serde-only: no `unsafe`, no macOS dependency. `cargo test --all` / `cargo
//! clippy --all-targets --locked` (`.github/workflows/ci.yml`) build this crate
//! on the ubuntu runner, so it must stay Linux-clean forever.
//!
//! # Row-at-a-time decode
//!
//! The wire is a bare JSON array, one object per account, and it must stay
//! decodable one element at a time: `FleetStatus.swift` decodes row-by-row so a
//! single malformed row (its doc-comment names an actual `"quota": null`
//! incident) cannot take down the whole fleet's view. [`AccountStatusRow`]
//! keeps that property expressible on the Rust side too — decode with
//! [`AccountStatusRow::from_value`] against one already-parsed `serde_json::Value`
//! at a time, never `serde_json::from_slice::<Vec<AccountStatusRow>>` against the
//! whole payload in one shot, which fails the entire batch on one bad row.

#![forbid(unsafe_code)]

pub mod cswap_v1;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// One bucket's token and cost totals. Every field is a plain sum over the
/// requests in the bucket, so a client can add two of these together.
///
/// `costUsd` is the API **list-price equivalent** — what this traffic would
/// have cost on the API — not a bill: the accounts behind this proxy are
/// subscriptions. It is `null`, never `0.0`, when the bucket served requests
/// and none of their models could be priced; `unpricedRequests` says how many
/// of `requests` are missing from the figure, so a partial total is never
/// mistaken for a complete one.
///
/// A bucket with NO requests reports `costUsd: 0.0`. Nothing served is a
/// measured zero, and `null` is reserved for the one case above — an idle
/// account used to report `today.costUsd: null` beside `lastHour.costUsd: 0`
/// for the very same absence of traffic.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageTotals {
    #[serde(default)]
    pub requests: u64,
    /// Base (non-cached) input tokens. Deliberately NOT the row-level
    /// `inputTokens`, which is the QUOTA counter and folds cache creation and
    /// cache reads into one number — those are separate billing dimensions and
    /// have to stay apart to be priced.
    #[serde(default)]
    pub input_tokens: u64,
    /// ALL cache-creation tokens, both TTLs — the same quantity the row-level
    /// [`AccountStatusRow::cache_creation_tokens`] carries, deliberately, so
    /// one key never means two things in one row. The 5-minute part is this
    /// minus [`Self::cache_creation_1h_tokens`].
    #[serde(default)]
    pub cache_creation_tokens: u64,
    /// The SUBSET of [`Self::cache_creation_tokens`] written under the extended
    /// 1-hour TTL, which bills at twice base input rather than 1.25x.
    #[serde(default)]
    pub cache_creation_1h_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    /// `null` when nothing in this bucket could be priced — see the struct docs.
    #[serde(default)]
    pub cost_usd: Option<f64>,
    #[serde(default)]
    pub unpriced_requests: u64,
    /// The reasoning tokens this bucket's requests reported: a Codex response's
    /// `output_tokens_details.reasoning_tokens`. tcr reads them from Codex responses only,
    /// so a Claude bucket stays at zero and the key is skipped there: every Anthropic row
    /// stays the bytes it was before the key existed. Absent decodes as `0`.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub reasoning_tokens: u64,
}

/// `skip_serializing_if` for a counter that is written only once it counts something.
const fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// [`UsageTotals`] for a bounded window, plus the instant the window opened.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageWindow {
    /// Unix milliseconds: the instant this window opened. For [`UsageRow::window`] that is
    /// the account's current 5-hour window, `fiveHourResetAtMs` minus 5h; for
    /// [`UsageRow::seven_day_window`] it is the weekly reset minus the window's length.
    pub since: i64,
    #[serde(flatten)]
    pub totals: UsageTotals,
}

/// One account's usage, aggregated by the proxy at request time.
///
/// Absent (`null`) means "not measured": the row came from a server built
/// before this existed, or from the offline path, which has no serving
/// counters at all. It never means zero usage.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageRow {
    /// The local calendar day of the machine the SERVER runs on.
    #[serde(default)]
    pub today: UsageTotals,
    /// This account's current 5-hour window. `null` when the server has not
    /// learned the window's reset, so its start cannot be named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<UsageWindow>,
    /// The trailing 60 minutes — burn rate is `lastHour.costUsd` per hour, by
    /// definition.
    #[serde(default)]
    pub last_hour: UsageTotals,
    /// `today`, split by model id.
    #[serde(default)]
    pub today_by_model: BTreeMap<String, UsageTotals>,
    /// This account's weekly window, opened at the weekly reset minus the window's
    /// length. Absent when the weekly reset is not known, so the window's start cannot
    /// be named — the same reason [`Self::window`] is absent — and on every row from a
    /// server that predates the key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seven_day_window: Option<UsageWindow>,
}

/// A gating window (`5h` or `7d`) currently holding an account out of rotation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeldWindowRow {
    /// `"5h"` or `"7d"`. On a Codex row that reported the window's length, the label
    /// follows the length instead: whole days `"{n}d"`, whole hours `"{n}h"`, else
    /// `"{n}m"`, so a 30-day window reads `"30d"`.
    pub window: String,
    pub reset_at_ms: i64,
    pub minutes_until_reset: i64,
}

/// A Codex account's credits, as ChatGPT reported them on a response or a usage probe.
///
/// Optional on the row ([`AccountStatusRow::credits`]) because a Codex row carries it only
/// once credits were reported, and an Anthropic row never does: absent means "never
/// reported", not "no credits". `balance` is the provider's own string, verbatim
/// (`"12.50"`), and is itself absent when the report carried no balance.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexCredits {
    pub has_credits: bool,
    pub unlimited: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub balance: Option<String>,
}

/// Whether `tcr run --codex` would pool a codex session through this proxy, and if not,
/// why: a machine `code` and the sentence the launch itself prints as `reason`.
///
/// A fleet-wide fact the CLI computes, never the server, and repeats on every Codex row
/// ([`AccountStatusRow::codex_route`]) as `serverSha` is repeated. Optional because a row
/// from a CLI that predates it carries none, and an Anthropic row never does. `pooled:
/// true` carries no `code` and no `reason`; `upstream` is present only when the server's
/// Codex upstream is not the default.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexRoute {
    pub pooled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
}

/// A Codex refusal generated by tcr, shared by its error bodies and traffic counters.
/// The serialized spelling is the error's `code`; it is also the `type` except for a
/// credential refusal (`proxy_error`) and model refusal (`invalid_request_error`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexRefusalKind {
    NoCodexAccount,
    UsageLimitReached,
    PoolUnavailable,
    CredentialRefused,
    ModelUnavailable,
}

impl CodexRefusalKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoCodexAccount => "no_codex_account",
            Self::UsageLimitReached => "usage_limit_reached",
            Self::PoolUnavailable => "pool_unavailable",
            Self::CredentialRefused => "credential_refused",
            Self::ModelUnavailable => "model_unavailable",
        }
    }

    pub const fn error_type(self) -> &'static str {
        match self {
            Self::CredentialRefused => "proxy_error",
            Self::ModelUnavailable => "invalid_request_error",
            Self::NoCodexAccount | Self::UsageLimitReached | Self::PoolUnavailable => self.as_str(),
        }
    }
}

/// What tcr did with Codex requests in the last ten minutes: how many it refused itself,
/// the latest refusal's error `type` and when, and how many it relayed on the client's
/// own credential instead of a pooled one.
///
/// Fleet-wide, counted by the serving process and repeated on every Codex row
/// ([`AccountStatusRow::codex_traffic`]). Optional because a server or a CLI that predates
/// it reports none, and an Anthropic row never carries it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexTraffic {
    #[serde(rename = "refused10m")]
    pub refused_10m: u64,
    /// A newer server may report a refusal this client does not name yet. Skip that optional
    /// label while retaining the counters, timestamp and the rest of the fleet's status.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_known_codex_refusal"
    )]
    pub last_refusal: Option<CodexRefusalKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_refusal_at_ms: Option<i64>,
    #[serde(rename = "unpooled10m")]
    pub unpooled_10m: u64,
}

fn deserialize_known_codex_refusal<'de, D>(
    deserializer: D,
) -> Result<Option<CodexRefusalKind>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let kind = Option::<String>::deserialize(deserializer)?;
    Ok(kind.and_then(|kind| {
        CodexRefusalKind::deserialize(serde::de::value::StringDeserializer::<D::Error>::new(kind))
            .ok()
    }))
}

/// One tool call still awaiting its `tool_result`, on the wire.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunningToolRow {
    pub tool: String,
    pub started_ms: i64,
    /// What this call is doing, for a panel row that would otherwise print the bare tool
    /// name: a Bash command, an `Agent`/`Task` subagent description, the task a `TaskOutput`
    /// waits on, a file path, a search pattern, a fetch HOST or a search query — per tool,
    /// capped at 120 characters. `None` for a tool with no rule, or when the input carried
    /// no such field. The full per-tool list is `ToolUseEvent::command_head` in the main
    /// crate (`src/session_wire.rs`), which builds every one of these. On the server it is
    /// never written to a log, though it persists in the session-wire snapshot
    /// (`~/.cache/tcr/session-wire.json`; see that module's own doc).
    pub command_head: Option<String>,
    /// A Bash tool's coarse category (`"wait"`, `"compound"`, `"search"`, `"git-net"`,
    /// `"git-local"`, `"build"`, `"other"`) — `None` for any other tool, or when
    /// `command_head` is `None`. `#[serde(default)]` so a server that has not shipped this
    /// field yet still decodes against a newer panel. See `src/session_wire.rs`'s
    /// `CommandClass` in the main crate for the classifier itself.
    #[serde(default)]
    pub command_class: Option<String>,
    /// The timeout the call carried, in seconds. `None` when it carried none, and the key
    /// is then skipped, so a Claude session's bytes are unchanged. No Claude call carries
    /// one, and neither does a Codex call through codex's default, interactive
    /// `exec_command`, which has no timeout argument. Only codex's one-shot `exec_command`,
    /// which managed requirements select, carries `timeout_ms`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
}

/// One completed tool call, for a session's "ten slowest" list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlowToolRow {
    pub tool: String,
    pub seconds: f64,
    /// Same meaning as [`RunningToolRow::command_head`] — the head the call carried while it
    /// was running, kept with the completed row.
    pub command_head: Option<String>,
    /// Same meaning and same `#[serde(default)]` reasoning as [`RunningToolRow::command_class`].
    #[serde(default)]
    pub command_class: Option<String>,
    pub ended_ms: i64,
}

/// One tool's aggregate call stats, for a session's (or the fleet's) "BY TOOL" breakdown.
/// `tool` is the wire's `tool_use.name` verbatim —
/// `Read`, `Grep`, `Glob` and `Edit` are deliberately NOT merged server-side; a panel groups
/// them if it wants to.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolBucketRow {
    pub tool: String,
    #[serde(default)]
    pub calls: u64,
    #[serde(default)]
    pub errors: u64,
    /// Median duration (seconds) over a bounded reservoir of the most recent completed
    /// calls — see `src/session_wire.rs` in the main crate.
    #[serde(default)]
    pub seconds_p50: f64,
    /// Completed calls of this tool whose duration was 60 seconds or more.
    #[serde(default)]
    pub over_one_minute: u64,
}

/// A session's tool-call aggregates.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionToolsRow {
    #[serde(default)]
    pub calls: u64,
    #[serde(default)]
    pub errors: u64,
    #[serde(default)]
    pub timeouts: u64,
    /// [`Self::timeouts`] split by what the command WAS — keyed on the command class
    /// (`wait`, `build`, `git-net`, …) for a classified `Bash` call and on the tool's own
    /// name (`Agent`) for everything else, so the map sums to `timeouts` and a panel's
    /// "TIMED OUT TODAY" section and its headline count cannot disagree. A `BTreeMap`
    /// because it is displayed: the key order is stable between snapshots.
    /// `#[serde(default)]` so a payload from a server built before this field existed
    /// decodes as "no split known" rather than a parse failure.
    #[serde(default)]
    pub timeouts_by_class: BTreeMap<String, u64>,
    /// The commands that timed out in this session, newest first, capped at 20 — the rows
    /// behind [`Self::timeouts_by_class`]. Same `#[serde(default)]` reasoning.
    #[serde(default)]
    pub timed_out: Vec<SlowToolRow>,
    /// Completed calls (any tool) whose duration was 60 seconds or more — the session-wide
    /// total; [`ToolBucketRow::over_one_minute`] carries the same count per tool.
    /// `#[serde(default)]` so a payload from a server built before this field existed decodes
    /// as "none known" rather than a parse failure.
    #[serde(default)]
    pub over_one_minute: u64,
    /// Tool calls still awaiting a `tool_result`, capped at 64 per session.
    #[serde(default)]
    pub running: Vec<RunningToolRow>,
    /// The count of [`Self::running`] entries whose `tool` is `Agent` or `Task` — a running
    /// subagent — so a panel can print "2 subagents" without scanning the list. `#[serde(default)]`
    /// so a payload from a server built before this field existed decodes as "no subagents known"
    /// rather than a parse failure.
    #[serde(default)]
    pub subagents_running: u64,
    /// The ten slowest completed tool calls, descending by `seconds`.
    #[serde(default)]
    pub slowest: Vec<SlowToolRow>,
    /// Per-tool breakdown for this session's "BY TOOL" section. `#[serde(default)]` so a
    /// payload from a server built before this field existed decodes as "no breakdown known"
    /// rather than a parse failure.
    #[serde(default)]
    pub by_tool: Vec<ToolBucketRow>,
}

/// Fleet-wide tool-call totals, summed server-side across every live session — so a panel's
/// headline and "BY TOOL" bars read from the SAME numbers as the per-session rows and cannot
/// disagree with them by re-summing client-side.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionsSummary {
    #[serde(default)]
    pub calls: u64,
    #[serde(default)]
    pub over_one_minute: u64,
    #[serde(default)]
    pub timeouts: u64,
    /// Fleet-wide sum of [`SessionToolsRow::timeouts_by_class`], summed server-side for the
    /// same reason [`Self::by_tool`] is: the panel's headline and its "TIMED OUT TODAY"
    /// section read one number rather than two client-side re-sums that can drift.
    #[serde(default)]
    pub timeouts_by_class: BTreeMap<String, u64>,
    #[serde(default)]
    pub by_tool: Vec<ToolBucketRow>,
    /// Sum of [`SessionRow::cost_usd`] across every live session — see that field's
    /// doc-comment for what the number means and does not mean.
    #[serde(default)]
    pub cost_usd: f64,
}

/// How hot the serving Mac's CPU runs and how hard its fans work, read by the proxy through
/// AppleSMC (the `tcr-smc` crate). Machine-wide, not per account, so it rides the status
/// payload's top level and `tcr sessions --json`'s object, never an account row.
///
/// Every field defaults, for the same skew reason as [`SessionsSummary`]: a later field must
/// not fail an older client's parse.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeatRow {
    /// The hottest CPU sensor in °C, always between 0.5 and 150. Absent when no CPU sensor
    /// read this time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_celsius: Option<f64>,
    /// One per fan whose speed read. Empty on a Mac with no fans.
    #[serde(default)]
    pub fans: Vec<FanRow>,
    /// The SMC's `Ftst` unlock flag: `true` while a fan writer holds it raised. Absent on a
    /// Mac without it, when it did not read, and from a server older than fan control.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ftst: Option<bool>,
}

/// One fan of [`HeatRow::fans`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FanRow {
    /// 0-based, the `n` in the SMC's `F{n}Ac`.
    #[serde(default)]
    pub index: u8,
    #[serde(default)]
    pub actual_rpm: f64,
    /// 0 when it did not read.
    #[serde(default)]
    pub min_rpm: f64,
    /// 0 when it did not read: "no maximum", not a ceiling of 0.
    #[serde(default)]
    pub max_rpm: f64,
    /// The fan's mode key says a program fixed its speed, so it no longer follows the heat.
    #[serde(default)]
    pub manual: bool,
    /// `F{n}Tg`, the speed the fan is told to hold. Absent when it did not read, and from a
    /// server older than fan control.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_rpm: Option<f64>,
}

/// What the serving proxy's fan controller is doing: the `fanControl` object, a sibling of
/// [`HeatRow`] on the status payload's top level and on `tcr sessions --json`'s object.
///
/// The contract with TcrBar's popover. `state` and `reason` are closed enums here; the
/// committed fixture `tests/fixtures/fan-control-contract.json` carries one sample per state
/// TcrBar draws, and both sides read it. `reason` and `detail` are always present, `null`
/// when there is none.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FanControlRow {
    /// The mode asked for, as the route and `tcr fan` took it.
    pub desired: FanDesiredRow,
    pub state: FanStateRow,
    /// Why, for `foreign` and `unavailable`; `null` otherwise.
    pub reason: Option<FanReasonRow>,
    /// The hot guard holds every fan at full speed.
    pub hot_guard: bool,
    /// The target last written to each fan this process holds; empty when it holds none.
    pub targets: Vec<FanTargetRow>,
    /// A sentence for a person, or `null`.
    pub detail: Option<String>,
    /// Milliseconds since the Unix epoch when the state last changed, or the controller last
    /// acted on a command (taking one does not move it). `tcr fan` waits for a value at or after
    /// the moment it sent its command.
    pub since_ms: i64,
}

/// [`FanControlRow::desired`]: one mode, tagged by `mode`. Whole rpm and whole °C.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "mode",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum FanDesiredRow {
    Auto,
    Fixed { rpm: u32 },
    FollowCpu { quiet_below_c: u8, full_at_c: u8 },
}

/// [`FanControlRow::state`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FanStateRow {
    /// Automatic is wanted and nothing is held.
    Off,
    Entering,
    Holding,
    Releasing,
    /// Another program has the fans; `reason` says which.
    Foreign,
    /// A read failed; nothing is written until one succeeds.
    Unknown,
    /// No manual mode can be held; `reason` says why.
    Unavailable,
    /// A hand-back failed; the helper hands back on its own after the connection closed.
    Stuck,
}

/// [`FanControlRow::reason`]. A plain reason is a string (`"macsFanControl"`); one that carries
/// data is a one-key object: `{"modeLocked": 1}` (the fan), `{"helperVersion": {"installed":
/// 2}}` (the helper's protocol; `{}` when its reply was not the contract's shape at all).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FanReasonRow {
    MacsFanControl,
    AnotherApp,
    NotSignedForFans,
    NoSmc,
    NoFans,
    HelperUnreachable,
    HelperNoReply,
    HelperSignature,
    HelperVersion {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        installed: Option<u64>,
    },
    /// The fan that would not leave automatic.
    ModeLocked(u64),
    Contended,
    HelperRefused,
    /// The proxy's fan controller stopped on an internal error; a restart starts it again.
    ControllerStopped,
}

/// One entry of [`FanControlRow::targets`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FanTargetRow {
    /// 0-based, as [`FanRow::index`].
    pub fan: u8,
    /// What the helper wrote, after its clamp.
    pub rpm: f64,
}

/// One live session on the `tcr status --json` wire's `sessions` array (F1,
/// `docs/design/panel-tabs.md`). One entry per session the proxy has seen in the last hour —
/// see `src/session_wire.rs` in the main crate for how it is built and bounded.
///
/// `#[serde(default)]` on every field but `sessionId` so a payload from a server built before
/// this row existed simply omits `sessions` entirely (the array field itself is
/// `#[serde(default)]` on the enclosing status payload, in `status.rs`) — never a hard parse
/// failure that drops a client back to a fabricated all-zeros offline snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRow {
    pub session_id: String,
    /// The account this session's most recent request served against, or `None` when it has
    /// never been attributed to one (e.g. the request carried no stable identity).
    #[serde(default)]
    pub account: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    pub first_seen_ms: i64,
    pub last_seen_ms: i64,
    #[serde(default)]
    pub requests: u64,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    /// Cache-write tokens (5-minute plus 1-hour TTL creation), kept apart from `input_tokens`
    /// — see `crate::session_wire::WireSession::input_tokens`'s doc-comment for why folding
    /// cache dimensions into `input_tokens` double-counts them against `cache_read_tokens`.
    /// `#[serde(default)]` so a payload from a server built before this field existed decodes
    /// as `0` rather than a parse failure.
    #[serde(default)]
    pub cache_creation_tokens: u64,
    #[serde(default)]
    pub tools: SessionToolsRow,
    /// Requests seen for this session in each of the last 30 wall-clock minutes, oldest
    /// first, exactly 30 entries — a ring buffer that decays to zeros as a session goes
    /// idle. `#[serde(default)]` so a payload from a server built before this field existed
    /// decodes as an empty sparkline rather than a parse failure.
    #[serde(default)]
    pub req_per_minute: Vec<u16>,
    /// This session's usage priced through `src/pricing.rs`, server-side, from a per-model
    /// token tally kept on the session — a session that spans two models sums each model's
    /// own price rather than being priced once against whichever model is current. Same
    /// list-price-equivalent meaning as [`UsageTotals::cost_usd`], but plain `f64` rather than
    /// `Option`: a model this proxy cannot price contributes `0.0` here rather than making the
    /// whole session's figure absent. `#[serde(default)]` so a payload from a server built
    /// before this field existed decodes as `0.0` rather than a parse failure.
    #[serde(default)]
    pub cost_usd: f64,
    /// How long this session's prompt cache stays warm after its last request, in seconds:
    /// 3600 or 300, the tier of the session's most recent cache write, or `None` before it has
    /// written one. A client computes the time left as `last_seen_ms` plus this, minus now.
    /// `#[serde(default)]` so a payload from a server built before this field existed decodes
    /// as `None`; the key is omitted when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_ttl_secs: Option<u32>,
    /// `"codex"` for a Codex session. Absent means Anthropic, which is also how a server
    /// that predates the key reads every session, so a Claude session's bytes are
    /// unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// How many of [`Self::requests`] no price covers: a request whose model has no known
    /// rate, and on a Codex session also one whose usage has not been priced because its
    /// turn is still streaming or ended without reporting any. [`Self::cost_usd`] keeps its
    /// meaning — such a request adds `0.0` to it — so a count here says the figure is a
    /// floor. Skipped at zero: every request priced, or a server that does not count them,
    /// so an absent key never proves the figure complete.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub unpriced_requests: u64,
    /// The reasoning tokens this session's responses reported, a Codex response's
    /// `output_tokens_details.reasoning_tokens`. Skipped at zero, which is every Claude
    /// session.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub reasoning_tokens: u64,
}

/// One account's row on the `tcr status --json` wire.
///
/// Field-for-field mirror of what `render_accounts_json` emits today — see that
/// function's doc-comments in `src/cli.rs` for why each field is shaped the way
/// it is (in particular, why several are `Option` rather than a fabricated `0`
/// or `false` on the offline path). This type does not change the contract; it
/// gives the existing contract a name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountStatusRow {
    /// `"live"` or `"offline"` — which process's numbers this row carries.
    pub source: String,
    /// Short git sha of the serving build, `None` when there is no serving
    /// process to report one (offline path).
    pub server_sha: Option<String>,
    pub server_dirty: Option<bool>,
    pub http1_only: bool,
    pub name: String,
    pub priority: i64,
    pub status: String,
    pub disabled: bool,
    pub control: bool,
    pub quota: Option<f64>,
    /// `"ok"`, `"near"`, or `"spent"` — see `quota_state_token`.
    pub quota_state: String,
    /// Kebab-case `GateReason` token: `"ok"`, `"hold"`, `"refused"`,
    /// `"five-hour"`, `"seven-day"`, `"fable-weekly"`, `"standard"`, `"login"`,
    /// `"rejected"`, `"disabled"`, `"reserved"`, or `"parked"`. `"refused"` is the
    /// short hold an upstream 403 arms; it self-frees like `"hold"`.
    ///
    /// `"rejected"` is the provider's own refusal, which means a different signal per
    /// provider: on an Anthropic row, Anthropic's `anthropic-ratelimit-unified-status:
    /// rejected` header. On a Codex row, a 429 that carried
    /// `x-codex-rate-limit-reached-type` reads as the full window that refused it
    /// (`"five-hour"` or `"seven-day"`, freeing at that window's reset, or with no time
    /// once that reset has passed and no probe has yet confirmed the roll), else as the
    /// live hold while it lasts, `"hold"` or, when an upstream 403 armed it,
    /// `"refused"`, and as `"rejected"` only when neither explains it.
    pub gate: String,
    pub five_hour: Option<f64>,
    pub seven_day: Option<f64>,
    pub seven_day_oi: Option<f64>,
    pub five_hour_state: Option<String>,
    pub seven_day_state: Option<String>,
    /// Per-window state for the Fable weekly (`seven_day_oi`), mirroring
    /// [`Self::seven_day_state`] and gating Fable requests only — see
    /// `GateReason::FableWeekly`. `null` when that window has no reading yet,
    /// same "not measured" idiom as the fields beside it.
    pub seven_day_oi_state: Option<String>,
    pub five_hour_reset_at_ms: Option<i64>,
    pub seven_day_reset_at_ms: Option<i64>,
    /// The Fable weekly window's reset, mirroring [`Self::seven_day_reset_at_ms`]:
    /// unconditional on threshold, `null` when the window's reset has already
    /// elapsed with nothing learned since or was never learned at all.
    pub seven_day_oi_reset_at_ms: Option<i64>,
    /// `None` on the offline path — a structural "not measured", never `0`.
    pub requests: Option<u64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_hit_ratio: Option<f64>,
    pub last_probe_ms: Option<i64>,
    /// When a quota reading was last taken, from a probe OR response headers, in
    /// Unix ms; `null` when nothing has been read. `lastProbeMs` cannot say how
    /// old the windows are: headers move them between probes, and a failed probe
    /// stamps it while reading nothing. `#[serde(default)]` so a row from an
    /// older server, which has no such key, decodes as `None`.
    #[serde(default)]
    pub quota_updated_at_ms: Option<i64>,
    pub probe_status: String,
    pub probe_error: Option<String>,
    pub stream_error_count: Option<u64>,
    pub last_stream_error: Option<String>,
    pub held: Vec<HeldWindowRow>,
    pub free_at_ms: Option<i64>,
    pub seconds_until_free: Option<i64>,
    pub rate_limited_until_ms: Option<i64>,
    pub groups: Vec<String>,
    pub reserved_groups: Vec<String>,
    /// The subset of [`Self::groups`] marked `parked`
    /// (`groupSettings.<g>.parked`) — a non-empty list means this account is
    /// held out of rotation entirely, the same way `disabled` does. Rides
    /// beside `reserved_groups` and for the same reason: the panel names the
    /// group that parked a row, which it cannot do from membership alone.
    /// `#[serde(default)]` so a row from a server predating the field decodes
    /// as "nothing parked" rather than failing.
    #[serde(default)]
    pub parked_groups: Vec<String>,
    /// The subset of [`Self::groups`] that have opted in to letting an explicit
    /// `--group` ask select the control account (`groupSettings.<g>.allowControlAccount`).
    /// Rides beside `reserved_groups` and for the same reason: the panel decides
    /// whether a group can route at all, and it cannot answer that from
    /// membership alone once the opt-in exists.
    #[serde(default)]
    pub control_allowed_groups: Vec<String>,
    /// Every group on the fleet mapped to its resolved color, repeated per row.
    pub group_colors: BTreeMap<String, String>,
    /// Cache-creation input tokens (a subset of `inputTokens`, like
    /// `cacheReadTokens` beside it). Tracked since cache accounting landed and
    /// never emitted here until now, which left `cacheReadTokens` on the wire
    /// with no companion to say how much of the input was spent WRITING the
    /// cache. `None` on the offline path, same "not measured" idiom as the
    /// other serving counters.
    #[serde(default)]
    pub cache_creation_tokens: Option<u64>,
    /// Proxy-computed usage and cost for this account. `None` when the serving
    /// build predates it or the row came from the offline path — see
    /// [`UsageRow`]'s doc-comment: absent is "not measured", never zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageRow>,
    /// `"codex"` for a Codex account; absent for Anthropic, so a client that predates the field
    /// (every TcrBar build before this change) decodes the row exactly as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    // The ten keys below are the facts a Codex account reports and an Anthropic one never
    // does. Each is optional and skipped when it holds nothing, so an Anthropic row
    // serializes exactly as it did before they existed, and a row from an older server
    // decodes with every one unknown.
    /// Which usage windows the plan reported, from `"fiveHour"` and `"sevenDay"`, in that
    /// order. Absent: unknown, so a panel draws both rows. `[]`: the plan reported no
    /// window at all, which is a fact, not a gap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_windows: Option<Vec<String>>,
    /// The length, in minutes, the 5-hour slot's window reported. Absent when no length
    /// was reported; the label is then `5h`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub five_hour_window_minutes: Option<u32>,
    /// The length, in minutes, the weekly slot's window reported. Absent when no length
    /// was reported; the label is then `7d`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seven_day_window_minutes: Option<u32>,
    /// Why ChatGPT refused this account, verbatim: `rate_limit_reached`, or a credits or
    /// spend-cap kind. Present exactly while a quota refusal stands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_reached_type: Option<String>,
    /// Whether the account's spend control is reached. Skipped when `false`, and the
    /// `default` is load-bearing on a non-`Option` field: without it, a row from a server
    /// that predates the key fails to decode at all (see the main crate's
    /// `status::AccountStatus::stream_error_count`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub spend_control_reached: bool,
    /// The account's credits, once reported. Absent when never reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credits: Option<CodexCredits>,
    /// Why this account's login is known dead: `refresh_token_reused`,
    /// `refresh_token_expired`, `refresh_token_invalidated`, `invalid_grant`, `http_401`,
    /// or another raw code. Absent when the login is not known dead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login_error: Option<String>,
    /// The absolute path of the folder this account's login was imported from. Absent for
    /// a login of tcr's own, and for one imported before the key existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_home: Option<String>,
    /// Refreshed tokens still need guarded publication to the imported Codex login.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub token_sync_pending: bool,
    /// Whether `tcr run --codex` would pool codex, the same on every Codex row. Filled by
    /// the CLI, never by the server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_route: Option<CodexRoute>,
    /// The last ten minutes of Codex refusals and unpooled requests, the same on every
    /// Codex row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_traffic: Option<CodexTraffic>,
    /// The organization's plan as the profile endpoint reported it, VERBATIM
    /// (`claude_max`, `claude_team`, `claude_pro`, `claude_enterprise`) — for a
    /// script that wants the provider's own word rather than our label. `None`
    /// when this account has never been profiled. On a Codex row it is the
    /// ChatGPT plan instead (`pro`, `plus`, …), the login's `chatgpt_plan_type`
    /// claim, equally verbatim.
    #[serde(default)]
    pub organization_type: Option<String>,
    /// The organization's rate-limit tier, verbatim
    /// (`default_claude_max_20x`, `default_raven`) — the field that separates
    /// Max 20x from Max 5x, on the wire for the same script-facing reason as
    /// [`Self::organization_type`].
    #[serde(default)]
    pub rate_limit_tier: Option<String>,
    /// The organization's seat tier, verbatim (`organization.seat_tier`) —
    /// `team_standard` and `team_tier_1` both observed on one live Team org.
    /// `None` on Max and Pro rows, which have no seats: the field is a property
    /// of a seated org, not of every account.
    #[serde(default)]
    pub seat_tier: Option<String>,
    /// The customer-facing plan label ("Max 20x", "Team Premium", ...) derived
    /// from the three raw fields above by [`plan_label`] — or, on a Codex row,
    /// from `organizationType` alone by [`codex_plan_label`] ("ChatGPT Pro") —
    /// the ONE place each mapping exists, so the CLI's plain text, the TUI and
    /// the macOS panel cannot disagree about what a row's plan is called.
    ///
    /// `None` when nothing is known, and that is load-bearing: a fabricated
    /// label is worse than a blank, because the whole point of this field is
    /// telling two rows with the SAME NAME apart.
    #[serde(default)]
    pub plan: Option<String>,
    /// The org this account is scoped to. A FACT about the row, for a client to
    /// show — not an address: `name` is unique and is the whole address every
    /// `tcr` verb takes.
    ///
    /// It used to be how a client narrowed a duplicated email, back when two
    /// rows could share a name and the panel's "Copy Access Token" failed with
    /// "matches 2 accounts". Naming the org is still worth doing on a row whose
    /// name does not already carry it.
    #[serde(default)]
    pub org_uuid: Option<String>,
    #[serde(default)]
    pub org_name: Option<String>,
}

/// The customer-facing plan label for one account, derived from the three raw
/// profile fields — `organization.organization_type`,
/// `organization.rate_limit_tier` and `organization.seat_tier`.
///
/// The single place this mapping exists. It lives in the wire crate rather than
/// the binary because both consumers need it: the server fills the `plan` field
/// from here, and so does the CLI's OFFLINE path, which has no server to ask.
///
/// `organization_type` is the PLAN WORD — Max, Pro, Team, Enterprise. The other
/// two supply at most one SUFFIX after it, and the rate-limit multiplier wins
/// when both could:
///
/// | plan word | suffix from | example |
/// |---|---|---|
/// | any | `rate_limit_tier` ending `_20x` / `_5x` | `Max 20x`, `Team 5x` |
/// | Team / Enterprise, no multiplier | `seat_tier` | `Team Standard`, `Team Tier 2` |
/// | Max / Pro, no multiplier | nothing | `Max`, `Pro` |
///
/// The multiplier outranks the seat because it is the stronger statement about
/// what the account can actually do. Measured on a live fleet: a premium Team
/// seat reads `seat_tier=team_tier_1` with `rate_limit_tier=default_claude_max_5x`,
/// while a standard one reads `seat_tier=team_standard` with
/// `rate_limit_tier=default_raven`. Labelling the first by its seat would print
/// "Team Tier 1", which tells a reader nothing; "Team 5x" tells them the size of
/// the account they are about to route traffic to.
///
/// `account.has_claude_max` is deliberately not an input — measured on the same
/// fleet it reads `true` on Team rows too, so a label derived from it would call
/// a Team account "Max".
///
/// Anything this function does not recognize survives VERBATIM rather than being
/// bucketed into the nearest known value: an unknown `organization_type` is
/// returned as-is with no suffix, and an unknown seat is appended raw (`Team
/// team_trial`) rather than dropped. A new plan or seat name is a thing we have
/// not seen yet; showing it unchanged is honest, mapping it onto "Max" or
/// silently discarding it is a claim nobody made. `None` in gives `None` out for
/// the same reason: an unprofiled account's plan is unknown, and "unknown" is
/// not "Pro".
pub fn plan_label(
    organization_type: Option<&str>,
    rate_limit_tier: Option<&str>,
    seat_tier: Option<&str>,
) -> Option<String> {
    /// The rate-limit multiplier a tier names, if it names one.
    fn multiplier(rate_limit_tier: Option<&str>) -> Option<&'static str> {
        let tier = rate_limit_tier?;
        if tier.ends_with("_20x") {
            Some("20x")
        } else if tier.ends_with("_5x") {
            Some("5x")
        } else {
            // A tier that names no multiplier (`default_raven`) is not a size
            // we failed to read — it is a tier from a different vocabulary, and
            // guessing a multiplier off it would be inventing a number.
            None
        }
    }

    /// What kind of seat this is, in words. `team_tier_1` → `Tier 1`; an
    /// unrecognized seat rides along verbatim rather than being dropped, since
    /// it is a real difference between two otherwise identical rows.
    fn seat_words(seat_tier: &str) -> String {
        if seat_tier.ends_with("_standard") {
            return "Standard".to_string();
        }
        match seat_tier.split("_tier_").nth(1) {
            Some(n) if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) => {
                format!("Tier {n}")
            }
            _ => seat_tier.to_string(),
        }
    }

    let org_type = organization_type?;
    let (plan, seated) = match org_type {
        "claude_max" => ("Max", false),
        "claude_pro" => ("Pro", false),
        "claude_team" => ("Team", true),
        "claude_enterprise" => ("Enterprise", true),
        // An unknown plan gets no suffix at all: we cannot know whether it is
        // seated, so neither refinement is safe to attach to it.
        other => return Some(other.to_string()),
    };

    Some(match multiplier(rate_limit_tier) {
        Some(mult) => format!("{plan} {mult}"),
        None => match seat_tier.filter(|_| seated) {
            Some(seat) => format!("{plan} {}", seat_words(seat)),
            None => plan.to_string(),
        },
    })
}

/// The customer-facing name of a ChatGPT plan, from the raw `chatgpt_plan_type` claim stored
/// verbatim in `organizationType` for Codex rows. A word this table has not met is named
/// `ChatGPT {word}`, the word kept as the provider wrote it (`edu` reads `ChatGPT edu`), so it
/// still reads as a ChatGPT plan beside the Claude ones rather than as a bare word. A word that
/// already starts with `ChatGPT`, in any case, is shown as it is, never prefixed twice.
pub fn codex_plan_label(raw: Option<&str>) -> Option<String> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    let lower = raw.to_ascii_lowercase();
    Some(match lower.as_str() {
        "free" => "ChatGPT Free".to_string(),
        "plus" => "ChatGPT Plus".to_string(),
        "pro" => "ChatGPT Pro".to_string(),
        "team" => "ChatGPT Team".to_string(),
        "business" => "ChatGPT Business".to_string(),
        "enterprise" => "ChatGPT Enterprise".to_string(),
        _ if lower.starts_with("chatgpt") => raw.to_string(),
        _ => format!("ChatGPT {raw}"),
    })
}

impl AccountStatusRow {
    /// Decode one already-parsed row. This is the row-at-a-time entry point —
    /// call it per-element over the array (`serde_json::Value::Array` iterated
    /// one item at a time, or one line of a JSONL rendering), never
    /// `serde_json::from_slice::<Vec<AccountStatusRow>>` against the whole
    /// payload, which lets one malformed row fail every row alongside it.
    pub fn from_value(value: serde_json::Value) -> Result<Self, serde_json::Error> {
        serde_json::from_value(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_traffic_tolerates_an_unknown_refusal_kind() {
        let traffic = serde_json::from_value::<CodexTraffic>(serde_json::json!({
            "refused10m": 1,
            "lastRefusal": "future_refusal",
            "lastRefusalAtMs": 1_800_000_000_000_i64,
            "unpooled10m": 0,
        }))
        .expect("a refusal added by a newer server must not break traffic decoding");
        assert_eq!(traffic.last_refusal, None);
        assert_eq!(traffic.refused_10m, 1);
        assert_eq!(traffic.last_refusal_at_ms, Some(1_800_000_000_000));
    }

    #[test]
    fn codex_traffic_refusal_kinds_keep_their_wire_strings() {
        for kind in [
            "no_codex_account",
            "usage_limit_reached",
            "pool_unavailable",
            "credential_refused",
        ] {
            let payload = serde_json::json!({
                "refused10m": 1,
                "lastRefusal": kind,
                "unpooled10m": 0,
            });
            let traffic: CodexTraffic = serde_json::from_value(payload.clone()).unwrap();
            assert_eq!(serde_json::to_value(traffic).unwrap(), payload);
        }
    }

    /// The three labels the live fleet actually produces, from the exact field
    /// triples measured on it — not synthetic combinations.
    #[test]
    fn plan_label_names_the_three_rows_the_live_fleet_produces() {
        assert_eq!(
            plan_label(Some("claude_max"), Some("default_claude_max_20x"), None),
            Some("Max 20x".to_string())
        );
        assert_eq!(
            plan_label(
                Some("claude_team"),
                Some("default_claude_max_5x"),
                Some("team_tier_1")
            ),
            Some("Team 5x".to_string()),
            "a premium Team seat carries a 5x multiplier, and the multiplier is \
             the useful half: 'Team Tier 1' names a seat nobody can size"
        );
        assert_eq!(
            plan_label(
                Some("claude_team"),
                Some("default_raven"),
                Some("team_standard")
            ),
            Some("Team Standard".to_string()),
            "a standard seat's tier names no multiplier, so the seat is what is \
             left to say"
        );
    }

    #[test]
    fn plan_label_falls_back_to_the_seat_only_when_no_multiplier_is_named() {
        assert_eq!(
            plan_label(Some("claude_team"), None, Some("team_tier_2")),
            Some("Team Tier 2".to_string()),
            "a seat tier with no multiplier beside it is spelled out"
        );
        assert_eq!(
            plan_label(Some("claude_team"), Some("default_raven"), None),
            Some("Team".to_string()),
            "no multiplier and no seat means no suffix — never a guessed Standard"
        );
        assert_eq!(
            plan_label(Some("claude_enterprise"), None, Some("enterprise_standard")),
            Some("Enterprise Standard".to_string())
        );
        assert_eq!(
            plan_label(Some("claude_enterprise"), Some("something_20x"), None),
            Some("Enterprise 20x".to_string())
        );
    }

    #[test]
    fn plan_label_ignores_a_seat_on_an_unseated_plan() {
        assert_eq!(
            plan_label(Some("claude_max"), None, Some("team_standard")),
            Some("Max".to_string()),
            "Max has no seats, so a stray seat value must not become part of its name"
        );
        assert_eq!(
            plan_label(Some("claude_pro"), None, Some("team_tier_1")),
            Some("Pro".to_string())
        );
        assert_eq!(
            plan_label(Some("claude_max"), Some("default_claude_max_5x"), None),
            Some("Max 5x".to_string())
        );
        assert_eq!(
            plan_label(Some("claude_max"), Some("something_else"), None),
            Some("Max".to_string()),
            "a tier naming no multiplier leaves the size off rather than guessing one"
        );
    }

    #[test]
    fn plan_label_returns_an_unknown_organization_type_verbatim() {
        assert_eq!(
            plan_label(
                Some("claude_something_new"),
                Some("default_claude_max_20x"),
                Some("team_standard")
            ),
            Some("claude_something_new".to_string()),
            "an unrecognized plan must survive verbatim, never be bucketed into a \
             known one — and it takes no suffix, because we cannot know whether it \
             is seated or what its tier vocabulary means"
        );
    }

    #[test]
    fn plan_label_returns_an_unknown_seat_tier_verbatim() {
        assert_eq!(
            plan_label(Some("claude_team"), None, Some("team_trial")),
            Some("Team team_trial".to_string()),
            "an unrecognized seat is a real difference between two rows — it rides \
             along verbatim rather than being dropped or read as standard"
        );
        assert_eq!(
            plan_label(Some("claude_team"), None, Some("team_tier_x")),
            Some("Team team_tier_x".to_string()),
            "a tier suffix that is not a number is not a tier number"
        );
    }

    #[test]
    fn plan_label_is_none_when_the_plan_is_unknown() {
        assert_eq!(
            plan_label(None, None, None),
            None,
            "an unprofiled account has no label — never a fabricated default"
        );
        assert_eq!(
            plan_label(None, Some("default_claude_max_20x"), Some("team_standard")),
            None,
            "neither refinement ever names a plan on its own: non-Max orgs carry \
             tiers, and a seat without a plan is not a plan"
        );
    }

    /// A session row from a server built before `cacheTtlSecs` existed decodes it as absent,
    /// and one that carries it keeps it through a serialize and decode. The JSON is written
    /// as literal text, not built from the type under test.
    #[test]
    fn session_cache_ttl_decodes_absent_and_round_trips() {
        let old = r#"{"sessionId":"s1","firstSeenMs":1,"lastSeenMs":2}"#;
        let row: SessionRow = serde_json::from_str(old).expect("old payload decodes");
        assert_eq!(row.cache_ttl_secs, None);
        assert!(
            !serde_json::to_string(&row)
                .expect("serializes")
                .contains("cacheTtlSecs"),
            "an absent tier is omitted, not sent as null"
        );

        let new = r#"{"sessionId":"s1","firstSeenMs":1,"lastSeenMs":2,"cacheTtlSecs":3600}"#;
        let row: SessionRow = serde_json::from_str(new).expect("new payload decodes");
        assert_eq!(row.cache_ttl_secs, Some(3600));
        let again: SessionRow =
            serde_json::from_str(&serde_json::to_string(&row).expect("serializes"))
                .expect("round trip decodes");
        assert_eq!(again.cache_ttl_secs, Some(3600));
    }

    #[test]
    fn codex_plan_label_names_the_chatgpt_plans_and_prefixes_an_unknown_word() {
        assert_eq!(
            codex_plan_label(Some("pro")).as_deref(),
            Some("ChatGPT Pro")
        );
        assert_eq!(
            codex_plan_label(Some("plus")).as_deref(),
            Some("ChatGPT Plus")
        );
        assert_eq!(
            codex_plan_label(Some("team")).as_deref(),
            Some("ChatGPT Team")
        );
        assert_eq!(
            codex_plan_label(Some("business")).as_deref(),
            Some("ChatGPT Business")
        );
        assert_eq!(
            codex_plan_label(Some("enterprise")).as_deref(),
            Some("ChatGPT Enterprise")
        );
        assert_eq!(
            codex_plan_label(Some("free")).as_deref(),
            Some("ChatGPT Free")
        );
        assert_eq!(
            codex_plan_label(Some("edu")).as_deref(),
            Some("ChatGPT edu"),
            "an unknown word is named as a ChatGPT plan, the word kept as written"
        );
        assert_eq!(
            codex_plan_label(Some("ChatGPT Edu")).as_deref(),
            Some("ChatGPT Edu"),
            "a word that already says ChatGPT is not prefixed twice"
        );
        assert_eq!(
            codex_plan_label(Some("chatgpt_edu")).as_deref(),
            Some("chatgpt_edu"),
            "in any case"
        );
        assert_eq!(codex_plan_label(None), None);
        assert_eq!(codex_plan_label(Some("  ")), None);
        assert_eq!(
            codex_plan_label(Some("Pro")).as_deref(),
            Some("ChatGPT Pro")
        );
    }

    /// An Anthropic account's row exactly as the status path renders it: the first row of the
    /// committed contract fixture, decoded one row at a time as `tests/round_trip.rs` decodes
    /// every row. This crate has no other way to build a whole row.
    fn committed_anthropic_row() -> AccountStatusRow {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/status-contract.json");
        let committed =
            std::fs::read_to_string(path).expect("committed contract fixture is readable");
        let rows: Vec<serde_json::Value> =
            serde_json::from_str(&committed).expect("fixture is a bare JSON array");
        let first = rows.into_iter().next().expect("fixture carries a row");
        assert!(
            first.get("provider").is_none(),
            "the fixture's first row is an Anthropic account: {first}"
        );
        AccountStatusRow::from_value(first).expect("fixture row decodes")
    }

    #[test]
    fn provider_is_absent_on_the_wire_for_anthropic_rows_and_present_for_codex() {
        let mut row = committed_anthropic_row();
        assert!(
            !serde_json::to_string(&row)
                .unwrap()
                .contains("\"provider\"")
        );
        row.provider = Some("codex".to_string());
        let json = serde_json::to_string(&row).unwrap();
        assert!(json.contains("\"provider\":\"codex\""));
        let back: AccountStatusRow = serde_json::from_str(&json).unwrap();
        assert_eq!(back.provider.as_deref(), Some("codex"));
    }
}
