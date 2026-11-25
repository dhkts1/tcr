//! The offline fleet snapshot and how a snapshot renders: the `tcr accounts` text lines and
//! the status rows behind `tcr status --json`.

use std::path::Path;
use std::sync::Arc;

use time::OffsetDateTime;

use super::{import_claude_code_login_if_empty, load_config, warn_if_no_accounts};
use crate::build_info::BuildInfo;
use crate::config::Config;
use crate::manager::Manager;
use crate::oauth::{NoRefresh, TokenRefresher};
use crate::probe::{LiveUsageProber, UsageProber};
use crate::provider::Provider;
use crate::stats::{AccountSnapshot, CodexFacts, QuotaState, StatsSnapshot};

/// Build an OFFLINE snapshot from `config`: a [`Manager`] with `config_path =
/// None` (so a probe's token refresh is NEVER written to disk) AND a [`NoRefresh`]
/// refresher (so a probe NEVER performs an OAuth refresh at all), optionally
/// probing every account's live quota first.
///
/// The no-refresh guarantee is load-bearing and independent of the no-persist one:
/// refresh tokens are SINGLE-USE, so a refresh from this second process would
/// revoke the running server's copy and kill the account — even though we would
/// never write the rotated token back. Accounts with a valid access token probe
/// normally; expired ones surface a visible probe error instead of refreshing.
///
/// Split out from [`list_accounts`] / [`status`] so tests can inject a scripted
/// [`UsageProber`] and a scripted [`TokenRefresher`] — asserting both that the
/// on-disk config is byte-unchanged (the no-race guarantee) and that an expired
/// account is handled by the injected refresher, never a hidden [`LiveRefresher`]
/// (the no-refresh guarantee). Production callers pass [`NoRefresh`].
pub async fn snapshot_offline(
    config: Config,
    refresher: Arc<dyn TokenRefresher>,
    prober: Arc<dyn UsageProber>,
    probe: bool,
) -> StatsSnapshot {
    // config_path = None → persist_now / persist_tokens are silent no-ops, and the
    // caller's refresher (production: NoRefresh) can never reach the OAuth token
    // endpoint, so a probe here can neither mutate the user's file nor revoke the
    // live server's tokens.
    let manager = Manager::new(
        config,
        refresher,
        prober,
        Arc::new(crate::warmer::LiveWarmer::new()),
        None,
    );
    if probe {
        manager.probe_all().await;
    }
    manager.snapshot(OffsetDateTime::now_utc())
}

/// Gating quota for one account row: the most-spent of the three known windows
/// (`5h`, `7d`, `7d_oi`), matching the rotation eligibility gate. `None` when
/// nothing has been learned yet.
///
/// NOTE: this is deliberately 3-window and stays cli-scoped — do NOT unify with
/// manager.rs's 2-window eligibility gate (`[five_hour, seven_day]`), which
/// intentionally excludes `7d_oi` to mirror `eligible`'s dims. The display gate
/// and the routing gate are different by design.
fn gating_quota(a: &AccountSnapshot) -> Option<f64> {
    [a.five_hour, a.seven_day, a.seven_day_oi]
        .into_iter()
        .flatten()
        .reduce(f64::max)
}

/// A gating window (5h or 7d) currently holding an account out of rotation: its
/// effective utilization is at/over the account's threshold and it still resets in
/// the future. Mirrors `Quota::is_near`'s dimensions (5h + 7d; the model-scoped
/// `7d_oi` never gates shared rotation, so it is not shown as a hold here).
struct HeldWindow {
    /// `5h` or `7d`; on a Codex row, the window's own length when it reported one
    /// ([`window_length_label`]).
    label: String,
    reset: OffsetDateTime,
}

/// A window's label from its length in minutes, the rule TcrBar labels a Codex window's row
/// by: whole days read `{n}d`, whole hours `{n}h`, anything else `{n}m`. So 300 is `5h`, 10080
/// is `7d`, and a 30-day window, 43200, is `30d`.
fn window_length_label(minutes: u32) -> String {
    if minutes.is_multiple_of(24 * 60) {
        format!("{}d", minutes / (24 * 60))
    } else if minutes.is_multiple_of(60) {
        format!("{}h", minutes / 60)
    } else {
        format!("{minutes}m")
    }
}

/// The windows holding account `a` out of rotation, evaluated against its own
/// `threshold`. `a.five_hour`/`a.seven_day` are already the EFFECTIVE utilizations
/// and `*_reset` the live (future-only) resets — both baked in at snapshot time —
/// so a held window always carries a future reset to show.
///
/// A Codex row labels each window by the length it reported, so a 30-day window reads
/// `30d` where the fixed label would claim `7d`; without a reported length, and on every
/// Anthropic row, the label is the fixed `5h` or `7d`.
fn held_windows(a: &AccountSnapshot, threshold: f64) -> Vec<HeldWindow> {
    let label = |fixed: &str, minutes: Option<u32>| match a.provider {
        Provider::Anthropic => fixed.to_string(),
        Provider::Codex => minutes.map_or_else(|| fixed.to_string(), window_length_label),
    };
    let mut held = Vec::new();
    for (label, util, reset) in [
        (
            label("5h", a.codex.five_hour_window_minutes),
            a.five_hour,
            a.five_hour_reset,
        ),
        (
            label("7d", a.codex.seven_day_window_minutes),
            a.seven_day,
            a.seven_day_reset,
        ),
    ] {
        if let (Some(u), Some(reset)) = (util, reset)
            && u >= threshold
        {
            held.push(HeldWindow { label, reset });
        }
    }
    held
}

/// A window's reset as a bare `HH:MMZ` wall clock. Rendered in UTC (the `Z` is
/// explicit): local-time rendering would need the `time` crate's `local-offset`
/// feature, which is unset and is unsound under the multi-threaded runtime anyway —
/// the countdown beside it is timezone-independent and carries the actionable "when".
pub(super) fn format_reset_clock(reset: OffsetDateTime) -> String {
    format!("{:02}:{:02}Z", reset.hour(), reset.minute())
}

/// Countdown from `now` to a future `reset`, one unit: minutes under two hours
/// (`+92m`), hours under two days (`+5h`), else days (`+3d`). Clamped at `+0m` so a
/// reset that just elapsed never reads negative.
pub(super) fn format_reset_countdown(reset: OffsetDateTime, now: OffsetDateTime) -> String {
    let mins = (reset - now).whole_minutes().max(0);
    if mins < 120 {
        format!("+{mins}m")
    } else if mins < 60 * 48 {
        format!("+{}h", mins / 60)
    } else {
        format!("+{}d", mins / (60 * 24))
    }
}

/// The ` held=<win> resets=<HH:MMZ>(<+countdown>)` segment(s) for an account's
/// held windows — one per held window, empty when the account holds none.
///
/// Retained deliberately: the text status line now renders per-window inline
/// `(+countdown)` tokens instead, but `held_windows` still backs the JSON
/// `held` array and this suffix stays available for that parity / future reuse.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "kept for the JSON `held` parity described above; only tests call it"
    )
)]
pub(super) fn held_suffix(a: &AccountSnapshot, threshold: f64, now: OffsetDateTime) -> String {
    let mut s = String::new();
    for h in held_windows(a, threshold) {
        s.push_str(&format!(
            " held={} resets={}({})",
            h.label,
            format_reset_clock(h.reset),
            format_reset_countdown(h.reset, now),
        ));
    }
    s
}

/// A short, grep-matchable token for an account's live [`QuotaState`] — surfaced
/// on the status line and in the JSON row so a held-but-has-headroom account
/// (`near`) is never mistaken for a truly spent one (`spent`). `ok` is an
/// in-rotation account comfortably under its threshold.
const fn quota_state_token(state: QuotaState) -> &'static str {
    match state {
        QuotaState::Normal => "ok",
        QuotaState::NearLimit => "near",
        QuotaState::Exhausted => "spent",
    }
}

/// Render one quota window as a `key=value` token: `5h=100%(+93m)`, `wk=67%`, or
/// `5h=n/a`. The inline `(+countdown)` appears whenever the window has a live
/// (future) reset — a window's reset is a property of the window, not of
/// whether it is currently a binding hold, so it is no longer gated on the
/// account's threshold.
fn render_window(
    label: &str,
    util: Option<f64>,
    reset: Option<OffsetDateTime>,
    now: OffsetDateTime,
) -> String {
    match util {
        None => format!("{label}=n/a"),
        Some(u) => {
            let hold = match reset {
                Some(r) if r > now => format!("({})", format_reset_countdown(r, now)),
                _ => String::new(),
            };
            format!("{label}={:.0}%{hold}", u * 100.0)
        }
    }
}

/// Minimum served requests before [`cache_hit_ratio`] reports a number rather
/// than no-signal.
///
/// Grounded in two read measurements, not taste. A measurement of hold
/// episodes put the median distinct divert destinations at 2.0, so
/// an account whose only traffic is "a few cold diverts" sits at single-digit
/// requests; a live read of the running fleet on 2026-08-15 (`tcr status
/// --json`) showed every account with a real, sustained cache ratio at 376+
/// requests, three orders of magnitude above that. 20 sits comfortably above
/// the divert-only noise floor and nowhere near real serving volume, so it
/// cannot suppress an actual measurement while it reliably catches the
/// near-empty-denominator case this constant exists to silence. This is a
/// design judgment (σ2 — architecturally grounded in real data, not a
/// controlled A/B against the incident it targets), not a derived fact.
pub(super) const CACHE_SIGNAL_FLOOR_REQUESTS: u64 = 20;

/// The account's prompt-cache hit ratio, or `None` when there is nothing
/// reliable to report — never a placeholder number.
///
/// Two distinct reasons collapse to the same `None`, deliberately: nothing to
/// divide by (`input_tokens == 0` — the offline / never-served case, the
/// original honesty fix) and too little served to trust the ratio
/// (`requests` under [`CACHE_SIGNAL_FLOOR_REQUESTS`] — an account whose only
/// real traffic was a handful of cold diverts, which is what "the low
/// `cacheHitRatio` means the cache is broken" falsely read as). Both are "not
/// a measurement", and the wire's existing contract — `null`, never a
/// placeholder `0.0` — already reads as unambiguously distinct from a real 0%
/// to every consumer (`offline_status_reports_null_not_zero_hit_ratio`), so
/// folding the floor into the same `None` needs no new field on the wire: a
/// no-signal account and an offline account both render `"cacheHitRatio":
/// null`, and a consumer that already treats null as "not measured" handles
/// both without a code change.
///
/// The ratio itself is unchanged from the original fix — `cache_read_tokens /
/// input_tokens`. `input_tokens`/`cache_read_tokens` are populated
/// exclusively from served-request usage (`Manager::update_usage`, called
/// only from real proxied responses in `proxy.rs`); the background quota
/// prober and keep-warm path update `apply_usage`/`update_quota` instead,
/// which never touch these counters (verified by reading every call site,
/// 2026-08-15). So the denominator was already inference-only traffic before
/// this change — the false "cache is broken" signal was a sample-size
/// problem, not a wrong-traffic-class problem, and the floor above is the
/// fix for it.
///
/// Takes `source` and checks it FIRST, ahead of the floor: an offline
/// snapshot's counters are structurally zero regardless of `requests`, so the
/// floor check alone happened to null them too — but relying on that
/// coincidence would leave the two guards silently redundant instead of each
/// covering the case it names. Checking `source` first also makes the
/// contract order-independent of any future change to what "offline" counts
/// as zero: the moment `source` says "not the serving process", nothing else
/// about the row's numbers matters.
fn cache_hit_ratio(a: &AccountSnapshot, source: StatusSource) -> Option<f64> {
    if source == StatusSource::Offline {
        return None;
    }
    if a.input_tokens == 0 || a.requests < CACHE_SIGNAL_FLOOR_REQUESTS {
        return None;
    }
    Some(a.cache_read_tokens as f64 / a.input_tokens as f64)
}

/// Render a [`StatsSnapshot`] as plain text — ONE LINE PER ACCOUNT — so the
/// output is greppable (`account NAME priority=P quota=Q% status=S ...`). The
/// ratatui TUI renderer is unusable for stdout, and per Gil's greppable-output
/// rule a naive grep must be able to match any single field.
///
/// `source` is taken rather than inferred because one token depends on it:
/// `stream_errors` is a SERVING counter, so an offline snapshot's zero is
/// structurally unmeasured (see [`StatusSource`]) and renders `n/a`, never `0`.
///
/// Takes no per-account threshold: the inline `(+countdown)` on each window
/// token is now a function of "does this window have a live reset", not of
/// whether it is a binding hold, so there is nothing left in this renderer to
/// gate on it. `held_windows`/`held[]` (JSON) keep their own threshold — a
/// different question this text line never answered.
///
/// A Codex row says so right after its name (` provider=codex`, the one
/// greppable provider marker), and a Claude row's line is the line it always
/// was. On a Codex row a window token reads `none` when the plan reported its
/// windows and this one is not among them (a Pro plan has no 5-hour window, a
/// Business plan may report none), while `n/a` keeps meaning a window expected
/// and not measured yet. A reading left over for a window the plan no longer
/// reports does not bring the window back: what the plan reports decides.
///
/// After `parked_groups=` a Codex row ends with what only it can say, in this
/// order and each only when it has something to say ([`codex_tail`]): why its
/// login died, the limit ChatGPT says it reached, a reached spend control, its
/// credits, that it is unreachable while reserved (a Codex request carries no
/// group that could reach it), and its workspace, the first 8 hex digits of its
/// ChatGPT workspace id. That last one is what tells two rows apart that share
/// one email in two ChatGPT workspaces.
pub fn render_accounts(snapshot: &StatsSnapshot, source: StatusSource) -> String {
    if snapshot.accounts.is_empty() {
        return "no accounts configured\n".to_string();
    }
    let now = OffsetDateTime::now_utc();
    let mut out = String::new();
    for a in &snapshot.accounts {
        let provider = match a.provider {
            Provider::Codex => " provider=codex",
            Provider::Anthropic => "",
        };
        let five_hour = unreported_window(a, "5h", "fiveHour")
            .unwrap_or_else(|| render_window("5h", a.five_hour, a.five_hour_reset, now));
        let seven_day = unreported_window(a, "wk", "sevenDay")
            .unwrap_or_else(|| render_window("wk", a.seven_day, a.seven_day_reset, now));
        // Fable's model-scoped weekly never gates the general view: no reset field,
        // no countdown, and the whole token is omitted when it was never learned.
        let fable = match a.seven_day_oi {
            Some(u) => format!(" fable={:.0}%", u * 100.0),
            None => String::new(),
        };
        // Prompt-cache hit ratio. `n/a` — never `0%` — when there is no signal
        // to report (R3: no NaN either): see [`cache_hit_ratio`] for the two
        // cases that collapse to it, matching the `5h=n/a` / `wk=n/a` idiom.
        // An OFFLINE snapshot's counters live in the server's process and are
        // structurally zero here, and rendering that as a measured 0% is
        // exactly the lie that hid a real prompt-cache catastrophe — same
        // reasoning extends to a live account whose only traffic was a
        // handful of cold diverts. Greppable `cache=NN%` token for parity
        // with the JSON field.
        let cache = match cache_hit_ratio(a, source) {
            None => " cache=n/a".to_string(),
            Some(ratio) => format!(" cache={:.0}%", ratio * 100.0),
        };
        // Stream failures this account's streams carried (decayed window): an
        // in-band SSE `error` event, or a stream that hit EOF without Anthropic's
        // `message_stop` terminator (recorded as `"truncated"`). This is the only
        // place that class of failure is visible at all.
        //
        // `n/a` — never `0` — on an OFFLINE snapshot, for the same reason
        // `cache=n/a` exists: the counter lives in the serving process, so a fresh
        // one reads structurally zero, and "0 stream errors" is an affirmative
        // all-clear in a way "0 requests" is not. Publishing an unmeasured
        // all-clear on an error counter is precisely the failure this field was
        // added to end.
        let stream_errors = match source {
            StatusSource::Offline => " stream_errors=n/a".to_string(),
            StatusSource::Live => format!(" stream_errors={}", a.stream_error_count),
        };
        // The latest error's `error.type`, alongside the count. Omitted entirely
        // when none has been seen, matching the `fable=` idiom — the count above
        // already distinguishes "none" from "not measured".
        let last_stream_error = match (source, a.last_stream_error.as_deref()) {
            (StatusSource::Live, Some(kind)) => format!(" last_stream_error={kind}"),
            _ => String::new(),
        };
        // WHEN it comes back — the question `status=` cannot answer. `free_at` is
        // the instant ALL of this account's active gates clear, so it never
        // promises a return the weekly cap will not honour.
        //
        // Omitted (not `n/a`) when there is no instant to name, matching the
        // `fable=`/`last_stream_error=` idiom: a terminal gate carries no
        // clear-instant, and neither does a gate whose reset upstream never
        // reported. Absent means "no time can be promised" — printing
        // `free_in=0s` there would read as "returns now".
        //
        // Terminal does NOT mean "only a human clears it", which this comment
        // used to say. LOGIN and `disabled` do need a person. REJECTED does not:
        // `Quota::drop_rejection_if_a_window_rolled` (`src/quota.rs`) drops it as
        // soon as a probe reads a shared window's utilization strictly below the
        // stored one, which is upstream reporting a new window. It is still
        // omitted here, because what that clears on is the next PROBE after the
        // roll, not the reset instant itself — an interval this code cannot name
        // in advance, and the whole reason `account_gate` reports `None`.
        let free_in = match a.free_at {
            Some(f) if f > now => format!(" free_in={}s", (f - now).whole_seconds().max(1)),
            _ => String::new(),
        };
        // Group labels, greppable via `groups=codereview,dev`. Omitted entirely
        // when the account carries none, matching the `fable=`/`last_stream_error=`
        // idiom for "nothing to say" rather than printing `groups=`.
        let groups = if a.groups.is_empty() {
            String::new()
        } else {
            format!(" groups={}", a.groups.join(","))
        };
        // Reserved subset of `groups=`, greppable via `reserved_groups=codereview`.
        // Omitted entirely when none of this account's groups is reserved — same
        // "nothing to say" idiom as `groups=` above.
        let reserved_groups = if a.reserved_groups.is_empty() {
            String::new()
        } else {
            format!(" reserved_groups={}", a.reserved_groups.join(","))
        };
        // Parked subset of `groups=`, greppable via `parked_groups=codereview`
        // — same "nothing to say" idiom as `reserved_groups=` above. A row with
        // this token is out of rotation; `state=`/`gate` says so too, this says
        // WHICH group did it.
        let parked_groups = if a.parked_groups.is_empty() {
            String::new()
        } else {
            format!(" parked_groups={}", a.parked_groups.join(","))
        };
        // The account's plan, greppable via `plan=Max 20x` (`plan=ChatGPT Pro`
        // on a Codex row) and named by the one method that names it anywhere
        // (`AccountSnapshot::plan_label`). Omitted entirely — never
        // `plan=unknown` — for an account that has never been profiled,
        // matching the `fable=`/`groups=` "nothing to say" idiom. This is the
        // only token that tells two rows sharing an email apart, so a
        // fabricated value here would be worse than silence.
        let plan = match a.plan_label() {
            Some(label) => format!(" plan={label}"),
            None => String::new(),
        };
        out.push_str(&format!(
            "account {}{} priority={}{} {} {}{}{} state={} status={}{} probe={}{}{}{}{}{}{}{}\n",
            a.name,
            provider,
            a.priority,
            plan,
            five_hour,
            seven_day,
            fable,
            cache,
            quota_state_token(a.quota_state),
            a.status,
            free_in,
            a.probe_status.as_str(),
            stream_errors,
            last_stream_error,
            if a.disabled { " disabled" } else { "" },
            groups,
            reserved_groups,
            parked_groups,
            codex_tail(a),
        ));
    }
    out
}

/// `{label}=none` for a Codex row whose plan reported its usage windows without `window`
/// (`"fiveHour"` or `"sevenDay"`), else `None`, leaving the token to [`render_window`]. An
/// unknown list is not a plan without the window. A Claude row has no such list; its one
/// such fact is a usage read that said `"five_hour": null`
/// ([`AccountSnapshot::five_hour_absent_on_plan`]), which makes its 5h token `5h=none`.
fn unreported_window(a: &AccountSnapshot, label: &str, window: &str) -> Option<String> {
    let reported = match a.provider {
        Provider::Anthropic => {
            return (window == "fiveHour" && a.five_hour_absent_on_plan)
                .then(|| format!("{label}=none"));
        }
        Provider::Codex => a.codex.quota_windows.as_ref()?,
    };
    (!reported.iter().any(|w| w == window)).then(|| format!("{label}=none"))
}

/// A Claude row's banked resets on its `tcr status` line: ` resets=weekly:1,fable:2(Dec 10)`,
/// each kind held with its count, then the earliest use-by among them (UTC day). Read but
/// ineligible is ` resets=unknown`, never `0`: Anthropic did not say what the account holds.
/// Eligible with none held is ` resets=0`. Nothing when never read.
pub(super) fn claude_resets_token(resets: Option<&tcr_status_wire::BankedResets>) -> String {
    let Some(resets) = resets else {
        return String::new();
    };
    if !resets.eligible {
        return " resets=unknown".to_owned();
    }
    let held = resets.held();
    if held.is_empty() {
        return " resets=0".to_owned();
    }
    let kinds: Vec<String> = held
        .iter()
        .map(|(kind, count, _)| format!("{}:{count}", kind.token()))
        .collect();
    let mut token = format!(" resets={}", kinds.join(","));
    if let Some(day) = held
        .iter()
        .filter_map(|(_, _, ends)| *ends)
        .min()
        .and_then(utc_day)
    {
        token.push_str(&format!("({day})"));
    }
    token
}

/// ` auto_reset=fable:applied`: the reset tcr spent by itself on this row in the last day,
/// and how that ended. Nothing when it spent none.
fn auto_reset_token(last: Option<&tcr_status_wire::AutoReset>) -> String {
    last.map_or_else(String::new, |last| {
        format!(" auto_reset={}:{}", last.kind.token(), last.outcome.token())
    })
}

/// `Dec 10`: the UTC day of a Unix-ms instant.
fn utc_day(ms: i64) -> Option<String> {
    let at = OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000).ok()?;
    let format = time::macros::format_description!("[month repr:short] [day padding:none]");
    at.format(&format).ok()
}

/// The tokens that end a Codex row's line, in this order, each only when it has something to
/// say: ` login_error=` (the raw error the login died with), ` limit_reached=` (the raw limit
/// ChatGPT says it reached), ` spend_control=reached`, ` credits=` (`unlimited`, else the
/// balance verbatim while the account has credits), ` reachable=no` (a reserved Codex row,
/// which no Codex request can reach) and ` workspace=` ([`workspace_token`]). Empty on a Claude
/// row, whatever its Codex facts hold.
pub(super) fn codex_tail(a: &AccountSnapshot) -> String {
    match a.provider {
        Provider::Anthropic => {
            return claude_resets_token(a.codex.banked_resets.as_ref())
                + &auto_reset_token(a.codex.auto_reset.as_ref());
        }
        Provider::Codex => {}
    }
    let mut tail = String::new();
    if let Some(resets) = &a.codex.reset_credits {
        tail.push_str(&format!(" resets={}", resets.available_count));
        if let Some(expiry) = resets.earliest_expiry_ms {
            tail.push_str(&format!(" reset_expires_at_ms={expiry}"));
        }
    }
    tail.push_str(&auto_reset_token(a.codex.auto_reset.as_ref()));
    if a.codex.token_sync_pending {
        tail.push_str(" token_sync=pending");
    }
    if let Some(code) = &a.codex.login_error {
        tail.push_str(&format!(" login_error={code}"));
    }
    if let Some(kind) = &a.codex.limit_reached_type {
        tail.push_str(&format!(" limit_reached={kind}"));
    }
    if a.codex.spend_control_reached {
        tail.push_str(" spend_control=reached");
    }
    match &a.codex.credits {
        Some(credits) if credits.unlimited => tail.push_str(" credits=unlimited"),
        Some(tcr_status_wire::CodexCredits {
            has_credits: true,
            balance: Some(balance),
            ..
        }) => tail.push_str(&format!(" credits={balance}")),
        _ => {}
    }
    if a.gate == crate::stats::GateReason::Reserved {
        tail.push_str(" reachable=no");
    }
    if let Some(workspace) = a.org_uuid.as_deref().and_then(workspace_token) {
        tail.push_str(&format!(" workspace={workspace}"));
    }
    tail
}

/// The first 8 hex digits of a ChatGPT workspace id, the dashes skipped, as TcrBar and the
/// re-login refusal show it; `None` when the id holds fewer than 8.
fn workspace_token(org_uuid: &str) -> Option<String> {
    let hex: String = org_uuid
        .chars()
        .filter(char::is_ascii_hexdigit)
        .take(8)
        .collect();
    (hex.len() == 8).then_some(hex)
}

/// Where a rendered fleet view's numbers came from.
///
/// The distinction is not cosmetic. Only [`StatusSource::Live`] carries real
/// serving counters (requests, tokens, cache hit ratio) — those live in the
/// running proxy's process. [`StatusSource::Offline`] is a fresh process's view:
/// its quota bars are real (they come from a live probe) but every counter is
/// structurally zero because nothing has served through it. Labelling the two
/// apart is the whole point: a structurally-zero counter must never again be
/// mistaken for a measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusSource {
    /// Read from the running proxy's `/_tcr/status` endpoint.
    Live,
    /// Computed in this process from the config, with no server to ask.
    Offline,
}

impl StatusSource {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Offline => "offline",
        }
    }
}

/// Render a [`StatsSnapshot`] as a JSON array, one object per account.
///
/// `source` and `serverSha` are stamped on EVERY row rather than wrapped around
/// the array: the output has always been a bare array, and a `jq '.[].name'`
/// that works today must keep working. One row is one account is one line to
/// grep.
///
/// `server` is `None` on the offline path, where there is no serving process
/// whose build could be reported — `serverSha` is then `null`, the same
/// "not measured" idiom `cacheHitRatio` uses, never a placeholder that reads
/// like a real sha.
///
/// It renders no Codex route or traffic: every Codex row comes out without
/// `codexRoute` and `codexTraffic`, as from a CLI that predates them. `tcr status
/// --json` itself renders through [`account_status_rows_at`], with both.
#[cfg(test)]
pub(super) fn render_accounts_json(
    snapshot: &StatsSnapshot,
    thresholds: &[f64],
    source: StatusSource,
    server: Option<&BuildInfo>,
    http1_only: bool,
    control: Option<&str>,
    group_colors: &std::collections::BTreeMap<String, String>,
) -> String {
    status_rows_json(account_status_rows(
        snapshot,
        thresholds,
        source,
        server,
        http1_only,
        control,
        group_colors,
    ))
}

/// The `tcr status --json` text of `rows`, as [`account_status_rows_at`] built
/// them.
pub(super) fn status_rows_json(rows: Vec<tcr_status_wire::AccountStatusRow>) -> String {
    let rows: Vec<serde_json::Value> = rows
        .into_iter()
        // Round-trip through `Value` rather than serializing the struct
        // directly: `serde_json::Value`'s map is a `BTreeMap`, so keys come
        // out alphabetically ordered. That matches this wire's output before
        // this struct existed (the committed fixture's key order), while
        // `AccountStatusRow`'s own field order stays whatever reads best in
        // this source file.
        .map(|row| serde_json::to_value(row).unwrap_or(serde_json::Value::Null))
        .collect();
    serde_json::to_string_pretty(&rows).unwrap_or_else(|_| "[]".to_string())
}

/// The typed rows behind [`render_accounts_json`], one per account in the
/// snapshot's order. Split out so `tcr list --json` projects the very rows
/// `tcr status --json` prints instead of building a second copy of them.
///
/// Like [`render_accounts_json`], it passes no Codex route or traffic. The list
/// drops every Codex row anyway ([`is_claude_row`]).
pub(super) fn account_status_rows(
    snapshot: &StatsSnapshot,
    thresholds: &[f64],
    source: StatusSource,
    server: Option<&BuildInfo>,
    http1_only: bool,
    control: Option<&str>,
    group_colors: &std::collections::BTreeMap<String, String>,
) -> Vec<tcr_status_wire::AccountStatusRow> {
    account_status_rows_at(
        OffsetDateTime::now_utc(),
        snapshot,
        thresholds,
        source,
        server,
        http1_only,
        control,
        group_colors,
        None,
        None,
    )
}

/// [`account_status_rows`] at a given clock, with the fleet's Codex route and
/// traffic: the typed rows `tcr status --json` prints through
/// [`status_rows_json`].
///
/// `now` is the clock that `held[].minutesUntilReset` and `secondsUntilFree`
/// are computed against. Taking it as a parameter lets a test compare a fixed
/// instant with a committed fixture, with no normalisation layer between them.
///
/// `codex_route` and `codex_traffic` are fleet-wide facts, repeated on every
/// Codex row the way `serverSha` and `groupColors` are repeated on every row.
///
/// No Codex key reaches an Anthropic row, neither these two nor the account's
/// own Codex facts, so such a row is the bytes it always was. The one
/// exception is `quotaWindows: ["sevenDay"]` on a row whose plan has no
/// five-hour window ([`AccountSnapshot::five_hour_absent_on_plan`]).
#[expect(
    clippy::too_many_arguments,
    reason = "the snapshot plus each piece of server state a status row renders"
)]
pub(super) fn account_status_rows_at(
    now: OffsetDateTime,
    snapshot: &StatsSnapshot,
    thresholds: &[f64],
    source: StatusSource,
    server: Option<&BuildInfo>,
    http1_only: bool,
    control: Option<&str>,
    group_colors: &std::collections::BTreeMap<String, String>,
    codex_route: Option<&tcr_status_wire::CodexRoute>,
    codex_traffic: Option<&tcr_status_wire::CodexTraffic>,
) -> Vec<tcr_status_wire::AccountStatusRow> {
    snapshot
        .accounts
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let quota = gating_quota(a);
            let threshold = thresholds.get(i).copied().unwrap_or(1.0);
            // The Codex keys are written on a Codex row only. An Anthropic row
            // takes the empty facts and no route or traffic whatever it was
            // given, so every one of its keys is skipped, except `quotaWindows`
            // on a plan with no five-hour window (Ultra): `["sevenDay"]`, so a
            // panel drops the 5h meter instead of drawing it as unmeasured.
            let (facts, row_route, row_traffic) = match a.provider {
                Provider::Anthropic => (
                    CodexFacts {
                        quota_windows: a
                            .five_hour_absent_on_plan
                            .then(|| vec!["sevenDay".to_string()]),
                        banked_resets: a.codex.banked_resets.clone(),
                        auto_reset: a.codex.auto_reset.clone(),
                        ..CodexFacts::default()
                    },
                    None,
                    None,
                ),
                Provider::Codex => (
                    a.codex.clone(),
                    codex_route.cloned(),
                    codex_traffic.cloned(),
                ),
            };
            let held: Vec<tcr_status_wire::HeldWindowRow> = held_windows(a, threshold)
                .into_iter()
                .map(|h| tcr_status_wire::HeldWindowRow {
                    window: h.label,
                    reset_at_ms: (h.reset.unix_timestamp_nanos() / 1_000_000) as i64,
                    minutes_until_reset: (h.reset - now).whole_minutes().max(0),
                })
                .collect();
            tcr_status_wire::AccountStatusRow {
                source: source.as_str().to_string(),
                // Which build produced these numbers. A script watching this
                // output can diff it against `git rev-parse --short HEAD` and
                // know, without an `lsof`, whether the server predates the fix
                // it is verifying.
                server_sha: server.map(|b| b.sha.clone()),
                server_dirty: server.and_then(|b| b.dirty),
                // Server-wide, repeated per row like `serverSha` above — whether
                // this server's upstream clients are forced onto HTTP/1.1. See
                // `Config::http1_only` and `Manager::http1_only`.
                http1_only,
                name: a.name.clone(),
                priority: a.priority,
                status: a.status.clone(),
                disabled: a.disabled,
                // Whether THIS row is the identity-bound control account
                // (`Config::control_account` / `Manager::control_name`). Reported
                // on the offline path too, deliberately — it is a config fact,
                // not a serving counter, so it does NOT get the
                // null-when-offline treatment `streamErrorCount` gets below;
                // see `StatusPayload::control`'s doc-comment for why.
                control: control == Some(a.name.as_str()),
                quota,
                quota_state: quota_state_token(a.quota_state).to_string(),
                // The server's own terminal-gate verdict
                // (`Manager::account_terminal_gate`/`account_gate`), including the
                // one TcrBar could not otherwise see: `rejected`, the provider's
                // own refusal. On an Anthropic row that is the
                // `anthropic-ratelimit-unified-status: rejected` header. On a
                // Codex row, a 429 parsed as `crate::quota::CodexQuotaRefusal`
                // from its body or reached-type header reads as the full
                // window that refused it, else as the live hold while it lasts
                // (`hold`, or `refused` when an upstream 403 armed it), and as
                // `rejected` only when neither explains it: the snapshot's
                // display-only remap, `Manager::codex_refusal_gate`, which
                // routing never reads. Before this field
                // existed, `status` was rewritten only for `Throttled` (see
                // `snapshot.rs`), so a rejected account still read `status:
                // "active"` and the panel drew it as eligible when the router will
                // never select it. Kebab-case values via `GateReason`'s own
                // `Serialize`; every one is listed in docs/troubleshooting.md, which
                // `stats::tests` keeps complete. New field, so an
                // older reader on either end is unaffected: a client built before
                // this shipped simply never looks at it.
                gate: gate_reason_token(a.gate),
                five_hour: a.five_hour,
                seven_day: a.seven_day,
                seven_day_oi: a.seven_day_oi,
                // Per-window state, additive alongside the existing combined
                // `quotaState` above (which stays the most-spent-of-both gating
                // verdict and is UNCHANGED by this field's addition — nothing
                // that reads `quota`/`quotaState` today observes a different
                // value). `null` when that window has no reading yet — same
                // "not measured" idiom as `serverSha`/`cacheHitRatio` elsewhere
                // in this row, never a fabricated "ok". Lets TcrBar tint each of
                // the two per-window quota bars independently instead of both
                // by the shared gating state, so a 7d-red account with an empty
                // 5h window doesn't paint its 5h bar red.
                five_hour_state: a.five_hour.map(|u| {
                    quota_state_token(QuotaState::from_utilization(Some(u), threshold)).to_string()
                }),
                seven_day_state: a.seven_day.map(|u| {
                    quota_state_token(QuotaState::from_utilization(Some(u), threshold)).to_string()
                }),
                // The Fable weekly window's per-window state, same shape and
                // threshold as `sevenDayState` beside it — mirrors the
                // `is_fable`-gated comparison `account_gate` makes
                // (`GateReason::FableWeekly`), just evaluated unconditionally
                // here rather than only when a request is Fable-scoped.
                seven_day_oi_state: a.seven_day_oi.map(|u| {
                    quota_state_token(QuotaState::from_utilization(Some(u), threshold)).to_string()
                }),
                // A window's reset is a property of the window, not of whether
                // it is currently pinning the account — UNCONDITIONAL, no
                // threshold gate. `held[]` below stays exactly as it is; it
                // answers a different question ("which window is holding this
                // account out of rotation"). `null` — never `0`, never a
                // fabricated instant — is the same "not measured" idiom as
                // `serverSha`/`cacheHitRatio`/`fiveHourState` above, and
                // collapses two distinct honest cases: the window's reset has
                // already elapsed with nothing learned since, or no reset was
                // ever learned. No `minutesUntilReset` companion (unlike
                // `held[]`) — a server-computed relative figure goes stale
                // between polls, so the client derives the countdown from the
                // timestamp against its own clock instead.
                five_hour_reset_at_ms: a
                    .five_hour_reset
                    .map(|r| (r.unix_timestamp_nanos() / 1_000_000) as i64),
                seven_day_reset_at_ms: a
                    .seven_day_reset
                    .map(|r| (r.unix_timestamp_nanos() / 1_000_000) as i64),
                // The Fable weekly window's reset, same unconditional shape as
                // `sevenDayResetAtMs` beside it.
                seven_day_oi_reset_at_ms: a
                    .seven_day_oi_reset
                    .map(|r| (r.unix_timestamp_nanos() / 1_000_000) as i64),
                // `None` — NEVER `0` — on the OFFLINE path, same idiom as
                // `streamErrorCount`/`cacheHitRatio` below and for the same
                // reason: these four are pure serving counters that live in
                // the SERVING process, so a fresh offline `Manager` reads
                // them structurally zero. That used to render as a real `0`,
                // and offline is not only the no-server case — `fetch_live_status`
                // falls back to it on NoAnswer/Unusable too (a slow, wedged,
                // or restarting proxy), which are ordinary states here and
                // ones a human or a `jq` one-liner polling this JSON every few
                // seconds would misread as "this account served nothing"
                // instead of "not measured right now". `fiveHour`/`sevenDay`/
                // `quota` above are deliberately NOT guarded the same way —
                // they come from a live probe the offline path genuinely runs
                // itself, not from the serving Manager's counters, so they are
                // real numbers on both paths.
                requests: match source {
                    StatusSource::Offline => None,
                    StatusSource::Live => Some(a.requests),
                },
                input_tokens: match source {
                    StatusSource::Offline => None,
                    StatusSource::Live => Some(a.input_tokens),
                },
                output_tokens: match source {
                    StatusSource::Offline => None,
                    StatusSource::Live => Some(a.output_tokens),
                },
                cache_read_tokens: match source {
                    StatusSource::Offline => None,
                    StatusSource::Live => Some(a.cache_read_tokens),
                },
                // Prompt-cache hit ratio (0.0-1.0): cache_read / input_total, and
                // `null` — NEVER a literal 0.0 — when there is no reliable signal:
                // offline (see the four fields above), or `inputTokens` is 0 and
                // there is nothing to divide by (also keeps NaN out — R3), or
                // `requests` sits under `CACHE_SIGNAL_FLOOR_REQUESTS` and the
                // ratio would be measuring a near-empty denominator. See
                // [`cache_hit_ratio`], which checks `source` FIRST.
                //
                // The null is the honesty fix, twice over. `source: "offline"`
                // means these counters come from a fresh process, not the serving
                // one, so they are structurally zero; emitting `0.0` there
                // published an unmeasured number as a measured "0% cache hits" for
                // every account forever, and that false zero is precisely why a
                // real prompt-cache catastrophe went unseen. The same false
                // reading recurs on a LIVE account whose only traffic was a
                // handful of cold diverts — a real but statistically meaningless
                // low ratio reads exactly like "the cache is broken" to anyone
                // watching this field. `null` says "not measured"; `source` says
                // which process would have measured it, when it could have.
                cache_hit_ratio: cache_hit_ratio(a, source),
                // When the background quota probe last ran for this account
                // (`AccountSnapshot::last_probe`), Unix milliseconds like every
                // other timestamp on this wire. Already crosses the
                // server<->CLI boundary (`status.rs`'s `AccountStatus`
                // reconstructs it) and the TUI already renders it
                // (`tui.rs`'s `ok 45s` age column) — this was simply never
                // added to the JSON output. It matters here specifically: an
                // idle account's quota probe runs on a randomized ~300s
                // cadence, but TcrBar polls this JSON every ~3s, so without
                // this field a stale probe (proxy wedged, probe target
                // unreachable) is invisible to any consumer — the quota bars
                // just silently stop moving with no timestamp to notice by.
                // Real on BOTH paths, like `fiveHour`/`quota` above: the
                // offline path runs its own live probe, so this is not a
                // serving counter and gets no null-on-offline guard.
                last_probe_ms: a
                    .last_probe
                    .map(|t| (t.unix_timestamp_nanos() / 1_000_000) as i64),
                // When a quota reading was last taken from EITHER source, the
                // probe or a served response's headers
                // (`AccountSnapshot::quota_updated_at`). `lastProbeMs` above
                // cannot tell a consumer how old the bars are: headers move them
                // between probes, and a failed probe stamps it while reading
                // nothing. Real on both paths, like `lastProbeMs`.
                quota_updated_at_ms: a
                    .quota_updated_at
                    .map(|t| (t.unix_timestamp_nanos() / 1_000_000) as i64),
                probe_status: a.probe_status.as_str().to_string(),
                probe_error: a.probe_error.clone(),
                // Decayed count of stream failures — an in-band SSE `error` event,
                // or a stream that hit EOF without `message_stop` (recorded as
                // `"truncated"`). `null`, NEVER 0, on the offline
                // path: the counter lives in the serving process, so a fresh one
                // is structurally zero, and an unmeasured `0` on an ERROR counter
                // publishes an all-clear nobody measured. Same "not measured"
                // idiom as `cacheHitRatio`; `source` says which process would
                // have measured it.
                stream_error_count: match source {
                    StatusSource::Offline => None,
                    StatusSource::Live => Some(a.stream_error_count as u64),
                },
                // The latest error's `error.type` (e.g. "overloaded_error"), or
                // null when none has been observed / nothing measured.
                last_stream_error: match source {
                    StatusSource::Offline => None,
                    StatusSource::Live => a.last_stream_error.clone(),
                },
                held,
                // WHEN THE ACCOUNT COMES BACK — the question `status` cannot
                // answer. `status: "throttled"` is a state; this is the instant.
                //
                // It lives here rather than in the TUI's Status cell because that
                // column is Length(9) and "throttled" is exactly 9 characters, so
                // a countdown there is clipped at every terminal width; and it is
                // `free_at`, not the raw hold, because free_at is the instant ALL
                // of this account's active gates clear. Rendering the hold alone
                // would promise a return the weekly cap will not honour.
                //
                // Null is honest and load-bearing: `account_gate` reports no
                // instant for a terminal state (REJECTED / LOGIN / disabled) and
                // none for a gate whose reset upstream never reported. Absent
                // means "no time can be promised", never "returns now".
                //
                // A REJECTED row is null for a different reason than the other
                // two, and this comment used to flatten them into "only a human
                // clears those". LOGIN and `disabled` do wait on a person. A
                // rejection clears itself, in
                // `Quota::drop_rejection_if_a_window_rolled` (`src/quota.rs`), on
                // the first probe that reads a shared window below its stored
                // utilization. Null because that instant depends on the probe
                // schedule rather than on the window's reset, so no field here
                // can name it — not because nothing will ever clear it.
                free_at_ms: a
                    .free_at
                    .map(|f| (f.unix_timestamp_nanos() / 1_000_000) as i64),
                seconds_until_free: a
                    .free_at
                    .filter(|f| *f > now)
                    .map(|f| (f - now).whole_seconds()),
                // The 429 hold specifically, so an operator can tell a short
                // transient park from a week-scale quota cap without inferring it
                // from `gate`.
                rate_limited_until_ms: a
                    .rate_limited_until
                    .map(|t| (t.unix_timestamp_nanos() / 1_000_000) as i64),
                // Group labels (config fact, not a serving counter) — always an
                // array, `[]` when unlabelled, real on BOTH paths like `control`
                // above. Never `null`: "this account has no groups" is known,
                // not unmeasured.
                groups: a.groups.clone(),
                // The subset of `groups` currently RESERVED (`tcr group
                // reserve`), sorted. Same never-`null` contract as `groups`
                // above — this is the field the sibling macOS panel decodes to
                // mark a section reserved; name and shape are a fixed contract,
                // do not change them.
                reserved_groups: a.reserved_groups.clone(),
                // The subset of `groups` currently PARKED (`tcr group park`),
                // sorted. Same never-`null` contract as `reservedGroups` above,
                // and the field the panel reads to name the group that parked a
                // row rather than merely dimming it.
                parked_groups: a.parked_groups.clone(),
                control_allowed_groups: Vec::new(),
                // Every group on the FLEET (not just this row's own groups)
                // mapped to its resolved color — server-wide like `http1Only`
                // above, repeated per row so the panel can tint any account's
                // tag without a second lookup. Always an object, `{}` when
                // there are no groups, never `null`: see
                // `Config::group_colors`'s doc-comment. Name and shape are a
                // fixed contract with the sibling macOS panel — do not change
                // them.
                group_colors: group_colors.clone(),
                // Cache-creation tokens, the companion `cacheReadTokens` never
                // had here: without it a reader can see how much input was
                // served FROM cache but not how much was spent WRITING it, and
                // the two together are what make a cache-hit ratio
                // interpretable. Same null-on-offline guard as the four serving
                // counters above — it is one of them, and has been tracked in
                // `AccountSnapshot` all along.
                cache_creation_tokens: match source {
                    StatusSource::Offline => None,
                    StatusSource::Live => Some(a.cache_creation_tokens),
                },
                // Proxy-computed usage and cost. `None` on the OFFLINE path for
                // the same reason as the counters above, and one more besides:
                // an offline manager has no ledger and has aggregated nothing,
                // so every figure in this object would be a structural zero —
                // a fabricated "$0.00 spent today" is precisely the false
                // measurement this whole object exists to replace.
                usage: match source {
                    StatusSource::Offline => None,
                    StatusSource::Live => a.usage.clone(),
                },
                // Which upstream the account belongs to: `"codex"` on a Codex
                // row and NO KEY AT ALL on an Anthropic one, never `null`, so
                // every row a panel built before this key could decode is the
                // same bytes it always was (the contract fixture pins both).
                // A config fact, real on both paths like `groups`.
                provider: match a.provider {
                    Provider::Anthropic => None,
                    Provider::Codex => Some(a.provider.as_str().to_string()),
                },
                // What a Codex account reports and an Anthropic one never does
                // (`CodexFacts`), copied from the snapshot. No `source` guard:
                // unlike the serving counters above, a fact nobody reported is
                // already `None` or `false` and its key is skipped, so it can
                // never read as a measurement. Empty on an Anthropic row, but
                // for `quotaWindows: ["sevenDay"]` when its plan has no 5h window.
                quota_windows: facts.quota_windows,
                five_hour_window_minutes: facts.five_hour_window_minutes,
                seven_day_window_minutes: facts.seven_day_window_minutes,
                limit_reached_type: facts.limit_reached_type,
                spend_control_reached: facts.spend_control_reached,
                credits: facts.credits,
                reset_credits: facts.reset_credits,
                banked_resets: facts.banked_resets,
                auto_reset: facts.auto_reset,
                login_error: facts.login_error,
                codex_home: facts.codex_home,
                token_sync_pending: facts.token_sync_pending,
                // Fleet-wide, repeated on every Codex row like `serverSha`:
                // whether `tcr run --codex` would pool codex, and what the
                // server did with Codex requests in the last ten minutes.
                codex_route: row_route,
                codex_traffic: row_traffic,
                // The plan, three raw fields and one label. Real on BOTH paths
                // like `groups`/`control` above: this is a config fact read
                // from disk, not a serving counter, so the offline path knows
                // it exactly as well as the server does and gets no
                // null-when-offline guard.
                //
                // The raw provider strings ride alongside the label so a script
                // can key on the vocabulary Anthropic actually uses
                // (`jq 'select(.organizationType == "claude_team")'`) without
                // parsing English that exists to be read by a human.
                organization_type: a.organization_type.clone(),
                rate_limit_tier: a.rate_limit_tier.clone(),
                seat_tier: a.seat_tier.clone(),
                plan_display_name: a.plan_display_name.clone(),
                // Derived HERE, once, by `AccountSnapshot::plan_label` — the
                // same method the TUI and the plain-text renderer call, so
                // three surfaces cannot disagree about what one row's plan is
                // named, and the one place an Anthropic row goes to
                // `tcr_status_wire::plan_label` and a Codex row to
                // `codex_plan_label`. `null` for an account that has never been
                // profiled: a fabricated label would defeat the entire point of
                // a field whose job is telling two identically-named rows apart.
                plan: a.plan_label(),
                // The org, so a client can SHOW which one this row belongs to.
                // It is not how the row is addressed — the name is. Real on
                // both paths, like `groups`.
                org_uuid: a.org_uuid.clone(),
                org_name: a.org_name.clone(),
            }
        })
        .collect()
}

/// The wire's kebab-case `gate` token for a [`crate::stats::GateReason`] —
/// `GateReason` itself is `#[serde(rename_all = "kebab-case")]`, so this reuses
/// that `Serialize` impl rather than hand-duplicating the mapping. An
/// enum-to-string serialization has no failure mode, so the `unknown` token is
/// unreachable; if a future variant ever broke that, it is logged at ERROR
/// instead of crashing the status path.
fn gate_reason_token(gate: crate::stats::GateReason) -> String {
    serde_json::to_value(gate)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| {
            tracing::error!("a gate reason did not serialize to a JSON string");
            "unknown".to_owned()
        })
}

/// `tcr accounts [--probe]` — list configured accounts (offline). With
/// `--probe`, first refresh every account's live quota (never persisted).
pub async fn list_accounts(config_path: &Path, probe: bool) -> anyhow::Result<()> {
    // Read-only verb: plain load, no clobber-warning (we never save).
    let config = load_config(config_path)?;
    let config = import_claude_code_login_if_empty(config_path, config).await;
    warn_if_no_accounts(&config);
    let prober = LiveUsageProber::new().with_codex_base(config.codex_upstream.clone());
    let snapshot = snapshot_offline(config, Arc::new(NoRefresh), Arc::new(prober), probe).await;
    // `tcr accounts` is the offline verb by construction — it builds its own
    // Manager and never asks the server — so its serving counters are structurally
    // zero and must render as unmeasured, not as a measurement.
    print!("{}", render_accounts(&snapshot, StatusSource::Offline));
    Ok(())
}
