//! The shared, live snapshot the TUI renders.
//!
//! [`Manager::snapshot`](crate::manager::Manager::snapshot) computes one of these
//! from the current account state at a caller-supplied `now`. Every quota figure
//! is evaluated live at that instant (see [`crate::quota::QuotaWindow::effective`]),
//! so a bar the TUI draws is never a stale cached value — that is the display half
//! of bug #2's fix.

use std::path::Path;

use time::OffsetDateTime;

use crate::probe::ProbeStatus;
use crate::provider::Provider;

/// One row of the recent-request ring buffer.
#[derive(Debug, Clone)]
pub struct RequestLogEntry {
    pub time: OffsetDateTime,
    pub method: String,
    pub path: String,
    pub status: u16,
    pub account: String,
    /// The provider the request was for, decided by its path as the proxy routed it. The
    /// dashboard's log shortens a Codex path to its tail, so it must not have to guess the
    /// provider back from the path it shortens.
    pub provider: Provider,
}

/// `path` with the home folder shown as `~`, for a line a person reads: `~/.codex` rather
/// than the full path. Only a whole leading component counts, so `/Users/example2` is not
/// inside `/Users/example`. With no home, or a home that is the root or empty, the path is
/// returned as it is.
pub fn short_path(path: &str, home: Option<&Path>) -> String {
    let Some(home) = home.filter(|home| home.parent().is_some()) else {
        return path.to_string();
    };
    match Path::new(path).strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.to_string(),
    }
}

/// How close an account is to its own switch threshold, evaluated live. Drives
/// an honest "near limit" / "exhausted" label: an account parked out of rotation
/// because it is near/over its weekly (or 5-hour) cap is still operationally
/// **active** — never the red `error` reserved for a dead credential.
///
/// Serde-derived so it crosses the status endpoint's wire ([`crate::status`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum QuotaState {
    /// Comfortably under the switch threshold — in normal rotation.
    #[default]
    Normal,
    /// At/over the switch threshold but under 100%: held out of rotation until
    /// the window resets, though the credential itself is fine.
    NearLimit,
    /// At/over 100% on a gating window — fully spent until it resets.
    Exhausted,
}

impl QuotaState {
    /// The one place the `>=1.0 => Exhausted`, `>=threshold => NearLimit`, else
    /// `Normal` rule lives. Takes an already-`Option`-flattened utilization —
    /// `None` (window has no reading yet) maps to `Normal`, matching the
    /// pre-existing gating computation in
    /// [`crate::manager::Manager::snapshot`], which folds a `None` window out of
    /// the `reduce(f64::max)` before ever reaching this match. Shared by the
    /// combined gating state (`snapshot.rs`) and the per-window state now
    /// surfaced in the accounts JSON (`cli.rs`), so the threshold logic exists
    /// exactly once.
    pub fn from_utilization(util: Option<f64>, threshold: f64) -> Self {
        match util {
            Some(u) if u >= 1.0 => QuotaState::Exhausted,
            Some(u) if u >= threshold => QuotaState::NearLimit,
            _ => QuotaState::Normal,
        }
    }
}

/// Why an account is currently out of rotation, mirroring the hard gates
/// [`crate::manager::Manager::eligible`] applies — the single source of truth is
/// [`crate::manager::Manager::account_gate`], which computes this alongside a
/// [`AccountSnapshot::free_at`] clear-instant. Soft pacing is deliberately NOT a
/// reason here: it only ever narrows an already-healthy account, never holds one
/// out, so a paced account still reads [`GateReason::Ok`].
///
/// Serde-derived so it crosses the status endpoint's wire ([`crate::status`]).
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum GateReason {
    /// In rotation — no hard gate is active.
    Ok,
    /// A rate-limit hold (429 `retry-after`) that has not yet lifted.
    Hold,
    /// Upstream answered 403, refusing our credential, and the short hold it
    /// arms has not yet lifted. Self-frees at the hold's end, like [`Self::Hold`].
    Refused,
    /// The shared 5-hour bucket is at/over threshold.
    FiveHour,
    /// The shared weekly bucket is at/over threshold.
    SevenDay,
    /// The model-scoped weekly (Fable `7d_oi`) bucket is at/over threshold — only
    /// ever surfaced for a Fable-scoped evaluation (`is_fable = true`).
    FableWeekly,
    /// A standard (API-key) token/request limit is at/over threshold — mirrors the
    /// standard branch of [`crate::quota::Quota::is_near`], so `account_gate`
    /// agrees with `eligible` on API-key accounts. Never fires for OAuth accounts
    /// (all standard fields `None`).
    Standard,
    /// A dead credential (`AccountStatus::Error`) — needs a re-login, never self-frees.
    Login,
    /// The provider itself refused this account. On an Anthropic account that is
    /// an `anthropic-ratelimit-unified-status: rejected` header; on a Codex account
    /// it is a 429 that carried `x-codex-rate-limit-reached-type`
    /// ([`crate::quota::is_codex_quota_refusal`]). Unlike a window it carries no
    /// reset to wait on, so it names no time. It still ends without a human: a usage
    /// probe that sees one of the account's windows roll clears it, on either
    /// provider, and on a Codex account so does a probe that reads the account
    /// allowed ([`crate::quota::Quota::apply_usage`]). Long held out of rotation by
    /// [`crate::manager::Manager::account_hard_ok`], it had no reason of its own
    /// here and so rendered as [`GateReason::Ok`].
    ///
    /// That is the verdict [`crate::manager::Manager::account_gate`] returns, and
    /// the one the pin and control gates read. A SNAPSHOT reports a Codex refusal
    /// more precisely ([`crate::manager::Manager::codex_refusal_gate`]): as the full
    /// window that refused it, freeing at that window's reset (or with no time once
    /// that reset has passed and no probe has yet confirmed the roll), else as the
    /// live hold while it lasts: [`Self::Hold`], or [`Self::Refused`] when an upstream
    /// 403 armed it. So on a Codex row this variant means a standing refusal that
    /// neither explains.
    Rejected,
    /// Operator-disabled — held out until re-enabled, never self-frees.
    Disabled,
    /// This account belongs to a group marked `reserved` (`tcr group reserve`)
    /// and the request that would have selected it did not ask for one of
    /// that account's groups. Unlike the other gates this one is a fact about
    /// the REQUEST's group ask, not purely about the account — see
    /// [`crate::manager::Manager::account_gate`]'s `group` parameter. Never
    /// self-frees on its own (only `tcr group unreserve` clears it), so it
    /// carries no clear-instant, same as [`Self::Disabled`].
    Reserved,
    /// This account belongs to a group marked `parked` (`tcr group park`), so
    /// it is out of rotation entirely — unlike [`Self::Reserved`] this is a
    /// fact about the ACCOUNT, independent of what the request asked for: an
    /// explicit `--group` ask for the parked group does not lift it. Never
    /// self-frees (only `tcr group unpark` clears it), so it carries no
    /// clear-instant. Reported only when the account is not ALSO
    /// [`Self::Disabled`] on its own, so the panel never says a row is
    /// parked-by-group when it was parked by hand.
    Parked,
}

/// A single account's live-computed view.
#[derive(Debug, Clone)]
pub struct AccountSnapshot {
    /// This account's rotation index: the one [`crate::manager::Manager::set_disabled`],
    /// [`crate::manager::Manager::set_current`], the session pins and the sessions use.
    ///
    /// [`StatsSnapshot::accounts`] leaves out rows deleted from the config while the process
    /// runs, and those rows keep their slots, so a row's position in that list equals its slot
    /// only until a row before it is deleted. Anything that acts on an account it found by its
    /// position in the list acts through this.
    ///
    /// Never on any wire: no status row, quota row or session row carries it, and a snapshot
    /// rebuilt from a status document numbers its rows by position.
    pub slot: usize,
    pub name: String,
    /// Which upstream this account belongs to, carried from
    /// [`crate::manager::AccountRuntime::provider`]. It decides which vocabulary
    /// the plan fields below speak, and so which function labels them — see
    /// [`Self::plan_label`].
    pub provider: Provider,
    /// The account's plan as the profile endpoint reported it, verbatim, plus
    /// the two fields that refine it — carried straight from
    /// [`crate::manager::AccountRuntime`]. On a Codex account the first is the
    /// ChatGPT plan from the login's `chatgpt_plan_type` claim and the other two
    /// are `None`. The customer-facing LABEL is derived from these by
    /// [`Self::plan_label`] one layer out, so this snapshot stays the provider's
    /// own word and exactly one place turns it into English. `None` means never
    /// profiled.
    pub organization_type: Option<String>,
    pub rate_limit_tier: Option<String>,
    pub seat_tier: Option<String>,
    /// The org this account is scoped to, carried from
    /// [`crate::manager::AccountRuntime`]. A fact a CLIENT can show beside the
    /// row; it is not how the row is addressed — `name` is unique, and every
    /// account verb (`tcr token`, `remove`, `enable`, `disable`, `priority`)
    /// takes that name alone.
    pub org_uuid: Option<String>,
    pub org_name: Option<String>,
    pub priority: i64,
    pub status: String,
    pub disabled: bool,
    /// 5-hour utilization (0.0–1.0), evaluated live — `None` if never learned.
    pub five_hour: Option<f64>,
    pub five_hour_reset: Option<OffsetDateTime>,
    /// Weekly utilization (0.0–1.0), evaluated live.
    pub seven_day: Option<f64>,
    pub seven_day_reset: Option<OffsetDateTime>,
    /// Model-scoped weekly utilization (Fable), evaluated live.
    pub seven_day_oi: Option<f64>,
    /// The Fable weekly window's reset instant, live like [`Self::seven_day_reset`]:
    /// `None` once the window's reset has elapsed with nothing learned since, or
    /// when it was never learned at all — never a past instant.
    pub seven_day_oi_reset: Option<OffsetDateTime>,
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Cache-read input tokens (a subset of `input_tokens`). The prompt-cache
    /// hit ratio the TUI and `tcr status` surface is `cache_read_tokens /
    /// input_tokens` (guarded when `input_tokens == 0`).
    pub cache_read_tokens: u64,
    /// Cache-creation input tokens (also a subset of `input_tokens`).
    pub cache_creation_tokens: u64,
    pub last_used: Option<OffsetDateTime>,
    /// If throttled, when the hold lifts (only while still in the future).
    pub rate_limited_until: Option<OffsetDateTime>,
    /// Health of the most recent background quota probe for this account.
    pub probe_status: ProbeStatus,
    /// When the most recent probe finished — the TUI shows its age so a stalled
    /// probe is visible rather than silently freezing the quota bars.
    pub last_probe: Option<OffsetDateTime>,
    /// When a quota reading was last taken, from a probe OR response headers
    /// ([`crate::quota::Quota::updated_at`]). Unlike [`Self::last_probe`] it says
    /// how old the windows shown beside it are. Display data only.
    pub quota_updated_at: Option<OffsetDateTime>,
    /// The most recent probe's error message, if it failed.
    pub probe_error: Option<String>,
    /// Live quota state vs this account's switch threshold — for an honest
    /// near-limit/exhausted label that never masquerades as an error.
    pub quota_state: QuotaState,
    /// Why this account is out of rotation right now (the latest-clearing hard
    /// gate), computed live via [`crate::manager::Manager::account_gate`] for the
    /// general (non-Fable) view. [`GateReason::Ok`] when in rotation.
    ///
    /// One remap, for display only: on a Codex account whose `account_gate` is
    /// [`GateReason::Rejected`], this reports the full window that refused it, with
    /// [`Self::free_at`] at that window's reset (`None` once that reset has passed,
    /// until a probe confirms the roll), else the live hold, [`GateReason::Hold`] or,
    /// when an upstream 403 armed it, [`GateReason::Refused`], with `free_at` at the
    /// hold's end while that hold lasts
    /// ([`crate::manager::Manager::codex_refusal_gate`]). The pin and control gates
    /// still read the unmapped verdict.
    pub gate: GateReason,
    /// When [`Self::gate`] clears — the instant ALL active hard gates have lifted.
    /// `None` when in rotation (`Ok`), when the gate never self-frees
    /// (`Login`/`Disabled`), or when a gating window has no known reset (unknown —
    /// we cannot promise a time). On a Codex refusal it is when the gate SHOWN
    /// clears, not the refusal: the refusing window's reset, after which the refusal
    /// stands until a probe confirms the roll, or the hold's end, after which a
    /// refusal no window explains still reads [`GateReason::Rejected`].
    pub free_at: Option<OffsetDateTime>,
    /// Count of stream failures this account's streams carried within the decay
    /// window (see `STREAM_ERROR_WINDOW_MS` in `manager/mod.rs`): an in-band SSE
    /// `error` event, or a stream that hit EOF without Anthropic's `message_stop`
    /// terminator — recorded with the fixed kind `"truncated"`, which covers a
    /// mid-stream transport death and the malformed/utf8 break alike. An `error`
    /// event takes precedence: EOF without `message_stop` is only counted as
    /// `"truncated"` when no more specific `error` was already recorded. Before
    /// that check existed, a truncated 200 with no in-band `error` event was
    /// booked as a clean serve and never reached this field at all.
    /// OBSERVABILITY ONLY: nothing in `select.rs` reads it, so it never gates
    /// rotation. On the wire deliberately (see `status.rs`'s module doc): it
    /// carries no credential material, and `tcr status --json` is how an
    /// operator sees the fleet — `tcr status --json | jq '.[].streamErrorCount'`,
    /// rendered by `cli::render_accounts_json`, with the plain-text `tcr status`
    /// carrying the same number as a `stream_errors=` token.
    ///
    /// That last sentence was FALSE when this field was introduced: the renderers
    /// did not exist, so the value reached the wire and stopped there. Both were
    /// added afterwards, which is what makes the claim true now. It is load-bearing
    /// — if a future change drops either renderer, delete the claim with it rather
    /// than leaving a rationale the code no longer earns.
    pub stream_error_count: usize,
    /// The most recent stream failure's kind: an Anthropic `error.type` (e.g.
    /// `"overloaded_error"`) from an in-band SSE `error` event, or the fixed
    /// string `"truncated"` when the stream hit EOF without `message_stop` and
    /// no more specific `error` was already recorded. Same on-the-wire decision
    /// as `stream_error_count`, and surfaced by the same two renderers
    /// (`lastStreamError` in JSON, the `last_stream_error=` token in text).
    pub last_stream_error: Option<String>,
    /// Group labels for this account, mirroring `AccountRuntime::groups` —
    /// empty (never `None`) when the config carried no `groups` key.
    pub groups: Vec<String>,
    /// The subset of [`Self::groups`] currently marked `reserved`
    /// (`tcr group reserve`), sorted. Always an array, `[]` when none — a
    /// config fact, never `null`. See [`GateReason::Reserved`].
    pub reserved_groups: Vec<String>,
    /// The subset of [`Self::groups`] currently marked `parked`
    /// (`tcr group park`), sorted. Always an array, `[]` when none — a config
    /// fact, never `null`, same contract as [`Self::reserved_groups`] beside
    /// it. Non-empty means this account is out of rotation; see
    /// [`GateReason::Parked`].
    pub parked_groups: Vec<String>,
    /// The subset of [`Self::groups`] that have opted in to letting an explicit
    /// `--group` ask select the control account (`tcr group allow-control`),
    /// sorted. Always an array, `[]` when none — a config fact, never `null`,
    /// same contract as [`Self::reserved_groups`] beside it.
    pub control_allowed_groups: Vec<String>,
    /// Proxy-computed usage and cost for this account: today, the current 5-hour
    /// window, the trailing hour, and today split by model.
    ///
    /// `None` means NOT MEASURED, never "served nothing". It is `None` on any
    /// snapshot built outside the serving process — a fresh offline `Manager`
    /// has no traffic to aggregate and no ledger attached — and on a row
    /// reconstructed from a server whose build predates the field. The same
    /// honest-null discipline `cacheHitRatio` follows: an unmeasured zero read
    /// as a measurement is how a real prompt-cache catastrophe went unseen.
    pub usage: Option<tcr_status_wire::UsageRow>,
    /// What this account reports as a Codex account, and an Anthropic one never
    /// does — see [`CodexFacts`]. `CodexFacts::default()`, nothing reported, on
    /// every Anthropic account.
    pub codex: CodexFacts,
}

/// What a Codex account reports and an Anthropic one never does: which quota
/// windows its plan has and how long they are, why ChatGPT refused it, its
/// credits and spend control, why its login is dead, and the folder its login
/// was imported from.
///
/// Grouped in one struct so an Anthropic row is the one line
/// `codex: CodexFacts::default()`, instead of eight fields every Anthropic
/// literal must spell out.
///
/// `None` and `false` mean "not reported", never "reported absent". The one
/// value that is a fact rather than a gap is `quota_windows: Some(vec![])`: a
/// plan that reported no usage window at all, which is not the same as one
/// whose windows are unknown (`None`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CodexFacts {
    /// The usage windows the plan reported, from `"fiveHour"` and `"sevenDay"`,
    /// in that order. `None`: unknown. `Some(vec![])`: the plan reported none.
    pub quota_windows: Option<Vec<String>>,
    /// The length, in minutes, the five-hour slot's window reported.
    pub five_hour_window_minutes: Option<u32>,
    /// The length, in minutes, the weekly slot's window reported.
    pub seven_day_window_minutes: Option<u32>,
    /// Why ChatGPT refused the account, verbatim (`rate_limit_reached`, or a
    /// credits or spend-cap kind), present exactly while the refusal stands.
    pub limit_reached_type: Option<String>,
    /// Whether the account's spend control is reached.
    pub spend_control_reached: bool,
    /// The account's credits, once reported.
    pub credits: Option<tcr_status_wire::CodexCredits>,
    /// Why the account's login is known dead (`refresh_token_reused`, …).
    pub login_error: Option<String>,
    /// The absolute path of the folder the account's login was imported from.
    pub codex_home: Option<String>,
}

impl AccountSnapshot {
    /// The customer-facing plan label every surface shows: the `plan` key of
    /// `tcr status --json`, the `plan=` token of its text form and the TUI's dim
    /// suffix, so the three cannot disagree. An Anthropic account is labelled from
    /// its three profile fields by [`tcr_status_wire::plan_label`], a Codex account
    /// from its ChatGPT plan alone by [`tcr_status_wire::codex_plan_label`]. The
    /// two vocabularies do not overlap: `plan_label` has never met `pro` and would
    /// pass it through as the bare raw word.
    pub fn plan_label(&self) -> Option<String> {
        match self.provider {
            Provider::Anthropic => tcr_status_wire::plan_label(
                self.organization_type.as_deref(),
                self.rate_limit_tier.as_deref(),
                self.seat_tier.as_deref(),
            ),
            Provider::Codex => tcr_status_wire::codex_plan_label(self.organization_type.as_deref()),
        }
    }

    /// Why this Codex account's login is dead, as one clause for a person: the refresh error
    /// it died with ([`CodexFacts::login_error`]), in words. `None` on an Anthropic account, and
    /// on a Codex account with no recorded error, which leaves the caller to say only that the
    /// account needs a re-login.
    ///
    /// The one place the clause is worded, so the dashboard's re-login hint and `tcr status`'s
    /// re-login line cannot disagree. A reused refresh token on a row imported from a codex
    /// folder names that folder (`home` shown as `~`, see [`short_path`]): ChatGPT refresh
    /// tokens are single-use, and the codex CLI sharing the login is what spent it first.
    pub fn relogin_why(&self, home: Option<&Path>) -> Option<String> {
        match self.provider {
            Provider::Anthropic => return None,
            Provider::Codex => {}
        }
        let code = self.codex.login_error.as_deref()?;
        Some(match code {
            "refresh_token_reused" => match self.codex.codex_home.as_deref() {
                Some(folder) => format!(
                    "the codex CLI in {} refreshed this shared login first",
                    short_path(folder, home)
                ),
                None => "another copy of this login refreshed first".to_string(),
            },
            "refresh_token_expired" => "the login expired".to_string(),
            "refresh_token_invalidated" => "the login was revoked".to_string(),
            "invalid_grant" => "ChatGPT rejected the refresh (invalid_grant)".to_string(),
            "http_401" => "ChatGPT answered the refresh with 401".to_string(),
            other => format!("ChatGPT refused the refresh ({other})"),
        })
    }
}

/// Whether a live session was keyed on a stable client identity (a Codex
/// request's `session-id` header, tier 0 and Codex paths only; x-api-key /
/// metadata.user_id), on a hash of its cacheable prefix (`system` + `tools`)
/// because it had no stronger identity, or had neither and served unpinned.
/// DISPLAY-only — never a routing input.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SessionKind {
    Stable,
    /// Pinned via `proxy::prefix_session_key` (private, see its doc-comment): no `x-api-key` /
    /// `metadata.user_id`, but the request had a `system` and/or `tools` field
    /// whose hash pins it (and byte-identical requests after it) to one
    /// account. Weaker than `Stable` — it identifies the REQUEST's cacheable
    /// prefix, not the client — but still a real pin, distinct from `Fallback`.
    Prefix,
    Fallback,
}

/// One live session's view: a short display id, the account it is PINNED to, the
/// account that actually served its most recent request, how many requests it has
/// served, and when it was last seen.
///
/// The two account fields are deliberately separate because they genuinely differ:
/// a session's pin is HELD while a single request is diverted elsewhere (a Fable
/// title call whose model-scoped weekly is spent, one request during a hold that
/// clears while the cache is still warm — see
/// [`crate::manager::Manager::select`]). Collapsing them made every such divert
/// look like the session had moved account, which is how a fleet measured at a
/// 1.70% switch rate read as "sessions keep jumping" in the TUI.
#[derive(Debug, Clone)]
pub struct SessionSnapshot {
    pub id: String, // short hex derived from the u64 session key (display only)
    /// Account name this session is currently PINNED to, read from the manager's
    /// affinity map — the sole authority on the pin. A session with no pin (an
    /// identity-less fallback serve, a seeded demo row) has no home, so this falls
    /// back to [`Self::last_served_account`].
    pub account: String,
    /// Account name that served this session's most recent request. Equal to
    /// [`Self::account`] on a normal serve; DIFFERENT exactly when that one request
    /// was diverted while the pin stayed put — so the divert stays observable
    /// instead of being hidden.
    pub last_served_account: String,
    pub requests: u64,
    pub last_seen: Option<OffsetDateTime>,
    /// The provider of the account this session is pinned to ([`Self::account`]), else of the
    /// one that last served it. A session belongs to one provider for its whole life, so the
    /// dashboard can mark a Codex session's group without guessing it from a name.
    pub provider: Provider,
    /// [`SessionKind::Stable`] when keyed on a stable client identity (x-api-key /
    /// `metadata.user_id`); [`SessionKind::Prefix`] when there was none but the
    /// request's `system`/`tools` hash pinned it anyway; [`SessionKind::Fallback`]
    /// when there was neither and the request served unpinned. Display provenance
    /// only. `Prefix` sessions have a real pin and group under their account in
    /// the TUI like `Stable` ones do; `Fallback` sessions would fold into one dim
    /// aggregate row, but as of this writing no `SessionSnapshot` ever actually
    /// carries `Fallback` — see `TreeRow::Unpinned`'s doc-comment in `tui.rs` for
    /// why. Routing is unchanged either way.
    pub kind: SessionKind,
}

/// The account serving each provider right now, as positions in [`StatsSnapshot::accounts`]:
/// the account that most recently served a request of that provider. `None` for a provider
/// that has served nothing since the process started, or whose serving row was deleted.
///
/// One per provider because a mixed fleet serves both at once: the account a Claude request
/// last went to says nothing about where codex's requests go, and a single index made one
/// provider's serving account vanish from the dashboard every time the other one served.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CurrentByProvider {
    pub anthropic: Option<usize>,
    pub codex: Option<usize>,
}

impl CurrentByProvider {
    /// The account serving `provider`.
    pub fn get(&self, provider: Provider) -> Option<usize> {
        match provider {
            Provider::Anthropic => self.anthropic,
            Provider::Codex => self.codex,
        }
    }

    /// Record `idx` as the account serving `provider`.
    pub fn set(&mut self, provider: Provider, idx: usize) {
        match provider {
            Provider::Anthropic => self.anthropic = Some(idx),
            Provider::Codex => self.codex = Some(idx),
        }
    }
}

/// The whole picture the TUI paints each tick.
#[derive(Debug, Clone)]
pub struct StatsSnapshot {
    pub accounts: Vec<AccountSnapshot>,
    /// Index of the account that most recently served a request, if any.
    pub current: Option<usize>,
    /// The account serving each provider, as positions in [`Self::accounts`], the space
    /// [`Self::current`] uses. Read it through [`Self::serving`].
    pub current_by_provider: CurrentByProvider,
    /// Most-recent-first request log.
    pub recent: Vec<RequestLogEntry>,
    /// Live per-session serving stats in a STABLE order (pinned account, then
    /// session id) — deliberately NOT recency, so a row holds its place instead of
    /// jumping to the top of the pane on every request.
    pub sessions: Vec<SessionSnapshot>,
    /// Live sessions keyed on the Claude Code `session_id` embedded in
    /// `metadata.user_id` (F1, `docs/design/panel-tabs.md`) — a different grain from
    /// [`Self::sessions`] above, which is keyed on the proxy's own affinity hash and exists
    /// for the TUI's pin display. This one carries tool-call timing and is what
    /// `tcr status --json`'s `sessions` array (via [`crate::status::StatusPayload`]) is built
    /// from. Reusing the wire type directly here, same as [`AccountSnapshot::usage`] beside it.
    pub wire_sessions: Vec<tcr_status_wire::SessionRow>,
    /// Fleet-wide tool-call totals summed across [`Self::wire_sessions`] — see
    /// [`tcr_status_wire::SessionsSummary`]'s doc-comment for why this is computed once
    /// server-side rather than re-summed by every client.
    pub wire_sessions_summary: tcr_status_wire::SessionsSummary,
}

impl StatsSnapshot {
    /// The account serving `provider`, as a position in [`Self::accounts`], the space
    /// [`Self::current`] uses: [`Self::current_by_provider`]'s entry, else [`Self::current`]
    /// when that account belongs to `provider`. So a snapshot that carries only `current` (one
    /// built by hand, or from a source that does not track a serving account per provider)
    /// still marks its serving row.
    pub fn serving(&self, provider: Provider) -> Option<usize> {
        self.current_by_provider.get(provider).or_else(|| {
            self.current.filter(|&idx| {
                self.accounts
                    .get(idx)
                    .is_some_and(|a| a.provider == provider)
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::GateReason;
    use super::*;
    use serde::de::{Deserialize as _, Visitor};
    use std::path::Path;

    /// What the probe deserializer below hands back: serde's own list of the
    /// enum's wire names, or a sign that the derive took some other path.
    #[derive(Debug)]
    enum Probe {
        Variants(&'static [&'static str]),
        Other(String),
    }

    impl std::fmt::Display for Probe {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Probe::Variants(names) => write!(f, "variant names {names:?}"),
                Probe::Other(msg) => f.write_str(msg),
            }
        }
    }

    impl std::error::Error for Probe {}

    impl serde::de::Error for Probe {
        fn custom<T: std::fmt::Display>(msg: T) -> Self {
            Probe::Other(msg.to_string())
        }
    }

    /// A deserializer that reads nothing. Its only job is to catch the
    /// `variants` slice `#[derive(Deserialize)]` passes to `deserialize_enum`:
    /// the derive builds that slice from the enum's variants after
    /// `rename_all`, so it grows by itself when a variant is added and nobody
    /// has to remember to extend a list here.
    struct VariantNames;

    impl<'de> serde::Deserializer<'de> for VariantNames {
        type Error = Probe;

        fn deserialize_any<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, Probe> {
            Err(Probe::Other(
                "the derive asked for something other than an enum".to_string(),
            ))
        }

        fn deserialize_enum<V: Visitor<'de>>(
            self,
            _name: &'static str,
            variants: &'static [&'static str],
            _visitor: V,
        ) -> Result<V::Value, Probe> {
            Err(Probe::Variants(variants))
        }

        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
            bytes byte_buf option unit unit_struct newtype_struct seq tuple
            tuple_struct map struct identifier ignored_any
        }
    }

    /// Every `gate` token `tcr status --json` can print, taken from the type.
    fn gate_tokens() -> Vec<&'static str> {
        match GateReason::deserialize(VariantNames) {
            Err(Probe::Variants(names)) => names.to_vec(),
            other => panic!("could not read GateReason's variant names: {other:?}"),
        }
    }

    /// The names serde deserializes are the names it serializes: each one
    /// round-trips through the real wire format to itself. Without this, a
    /// `#[serde(rename(serialize = ...))]` could make the probe list differ
    /// from what `tcr status --json` prints.
    #[test]
    fn gate_tokens_are_the_serialized_wire_tokens() {
        let tokens = gate_tokens();
        // Positive control: the probe sees known variants, so an empty or
        // wrong list cannot pass the doc test below by matching nothing.
        assert!(tokens.contains(&"ok"), "{tokens:?}");
        assert!(tokens.contains(&"five-hour"), "{tokens:?}");
        assert!(tokens.contains(&"refused"), "{tokens:?}");
        for token in tokens {
            let gate: GateReason = serde_json::from_value(serde_json::json!(token))
                .unwrap_or_else(|err| panic!("{token} does not deserialize: {err}"));
            assert_eq!(
                serde_json::to_value(gate).expect("GateReason serializes"),
                serde_json::json!(token),
                "{gate:?} serializes to a different token than it deserializes from"
            );
        }
    }

    /// docs/troubleshooting.md has a table row for every gate value. A new
    /// `GateReason` variant fails this test until its row is written, because
    /// the token list comes from the type, not from this file.
    #[test]
    fn troubleshooting_doc_has_a_row_for_every_gate_token() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/docs/troubleshooting.md");
        let doc =
            std::fs::read_to_string(path).unwrap_or_else(|err| panic!("cannot read {path}: {err}"));
        let missing: Vec<&str> = gate_tokens()
            .into_iter()
            .filter(|token| {
                let row = format!("| `{token}` |");
                !doc.lines().any(|line| line.starts_with(&row))
            })
            .collect();
        assert!(
            missing.is_empty(),
            "docs/troubleshooting.md has no `| `<token>` |` table row for gate token(s) {missing:?}"
        );
    }

    /// The home folder reads `~`, and only as a whole leading component: a sibling folder whose
    /// name merely starts with the home's is not inside it.
    #[test]
    fn short_path_shows_home_as_a_tilde() {
        let home = Some(Path::new("/Users/example"));
        assert_eq!(short_path("/Users/example/.codex", home), "~/.codex");
        assert_eq!(short_path("/opt/codex", home), "/opt/codex");
        // A sibling whose name merely starts with the home's. Shown under /srv because the
        // repository's disclosure gate refuses a /Users/<name>/ path for any name but a few
        // fixture ones, and a sibling of /Users/example cannot be one of them.
        let srv = Some(Path::new("/srv/example"));
        assert_eq!(
            short_path("/srv/example2/.codex", srv),
            "/srv/example2/.codex"
        );
        assert_eq!(short_path("/srv/example/.codex", srv), "~/.codex");
        assert_eq!(
            short_path("/Users/example/.codex", None),
            "/Users/example/.codex"
        );
    }

    /// A signed-out Codex row, its login dead with `login_error`, imported from `codex_home`.
    fn signed_out_codex_row(
        login_error: Option<&str>,
        codex_home: Option<&str>,
    ) -> AccountSnapshot {
        AccountSnapshot {
            slot: 0,
            name: "erin@example.com".to_string(),
            provider: Provider::Codex,
            organization_type: Some("team".to_string()),
            rate_limit_tier: None,
            seat_tier: None,
            org_uuid: None,
            org_name: None,
            priority: 0,
            status: "error".to_string(),
            disabled: false,
            five_hour: None,
            five_hour_reset: None,
            seven_day: None,
            seven_day_reset: None,
            seven_day_oi: None,
            seven_day_oi_reset: None,
            requests: 0,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            last_used: None,
            rate_limited_until: None,
            probe_status: ProbeStatus::Error,
            last_probe: None,
            quota_updated_at: None,
            probe_error: None,
            quota_state: QuotaState::Normal,
            gate: GateReason::Login,
            free_at: None,
            stream_error_count: 0,
            last_stream_error: None,
            groups: Vec::new(),
            reserved_groups: Vec::new(),
            parked_groups: Vec::new(),
            control_allowed_groups: Vec::new(),
            usage: None,
            codex: CodexFacts {
                login_error: login_error.map(str::to_string),
                codex_home: codex_home.map(str::to_string),
                ..CodexFacts::default()
            },
        }
    }

    #[test]
    fn provider_dispatch_keeps_plan_and_login_metadata_separate() {
        let mut account = signed_out_codex_row(Some("refresh_token_expired"), None);
        account.organization_type = Some("pro".into());
        for (provider, plan, why) in [
            (Provider::Anthropic, "pro", None),
            (Provider::Codex, "ChatGPT Pro", Some("the login expired")),
        ] {
            account.provider = provider;
            assert_eq!(account.plan_label().as_deref(), Some(plan), "{provider:?}");
            assert_eq!(account.relogin_why(None).as_deref(), why, "{provider:?}");
        }
    }

    /// Why a Codex login died, one sentence per refresh error, in the words the dashboard's
    /// re-login hint and `tcr status` both print. A reused refresh token names the codex CLI's
    /// folder when the row was imported from one, because that CLI is what refreshed first.
    #[test]
    fn the_relogin_why_follows_the_login_error() {
        let home = Some(Path::new("/Users/example"));
        let why = |login_error: Option<&str>, codex_home: Option<&str>| {
            signed_out_codex_row(login_error, codex_home).relogin_why(home)
        };
        assert_eq!(
            why(Some("refresh_token_reused"), Some("/Users/example/.codex")).as_deref(),
            Some("the codex CLI in ~/.codex refreshed this shared login first")
        );
        assert_eq!(
            why(Some("refresh_token_reused"), None).as_deref(),
            Some("another copy of this login refreshed first")
        );
        for codex_home in [Some("/Users/example/.codex"), None] {
            for (login_error, sentence) in [
                ("refresh_token_expired", "the login expired"),
                ("refresh_token_invalidated", "the login was revoked"),
                (
                    "invalid_grant",
                    "ChatGPT rejected the refresh (invalid_grant)",
                ),
                ("http_401", "ChatGPT answered the refresh with 401"),
                (
                    "token_revoked_by_admin",
                    "ChatGPT refused the refresh (token_revoked_by_admin)",
                ),
            ] {
                assert_eq!(
                    why(Some(login_error), codex_home).as_deref(),
                    Some(sentence),
                    "{login_error} with codex_home {codex_home:?}"
                );
            }
            assert_eq!(why(None, codex_home), None, "no login error, no why");
        }

        let mut anthropic = signed_out_codex_row(Some("refresh_token_reused"), None);
        anthropic.provider = Provider::Anthropic;
        assert_eq!(
            anthropic.relogin_why(home),
            None,
            "an Anthropic row has no Codex login to explain"
        );
    }
}
