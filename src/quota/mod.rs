//! Per-account quota, learned passively from upstream response headers.
//!
//! This is where **bug #2 is designed out**: a quota bar that keeps the
//! last-learned utilization until the next request re-learns it lets a window
//! that has already passed its reset read as still-full. Here a window stores
//! its `reset` timestamp and [`QuotaWindow::effective`] computes utilization
//! **live** against `now`: once `now > reset`, the window reads as a fresh `0.0`.
//! Nothing is mutated on a tick — the freshness is a pure function of the clock,
//! so the TUI (and selection) can never show or act on a stale full bar.

use time::{Duration, OffsetDateTime};

/// Nominal length of the shared 5-hour session window.
const FIVE_HOUR: Duration = Duration::hours(5);
/// Nominal length of the weekly windows (`7d` and the model-scoped `7d_oi`).
const SEVEN_DAY: Duration = Duration::days(7);

/// Decide a window's `reset` when learning a fresh utilization, so a window is
/// never pinned out of rotation forever and never wrongly reads as instantly
/// fresh (bugs #4/#5 and #8):
/// - an explicit reported reset (past OR future) is authoritative;
/// - otherwise a prior reset is inherited only while still in the future;
/// - otherwise a bounded reset is synthesized `window` ahead of `now`, so a
///   utilization reported without any reset gates for at most one window length
///   and then reads fresh — never permanently, never immediately.
fn resolve_reset(
    reported: Option<OffsetDateTime>,
    prior: Option<OffsetDateTime>,
    now: OffsetDateTime,
    window: Duration,
) -> Option<OffsetDateTime> {
    if let Some(reset) = reported {
        return Some(reset);
    }
    if let Some(prior) = prior
        && prior > now
    {
        return Some(prior);
    }
    Some(now + window)
}

/// One rate-limit window: a utilization fraction plus when it resets.
#[derive(Debug, Clone, Copy)]
pub struct QuotaWindow {
    /// Utilization as reported (0.0–1.0; can exceed 1.0 in overage).
    pub utilization: f64,
    /// When this window resets. `None` means "unknown reset" (rare; keep util).
    pub reset: Option<OffsetDateTime>,
}

impl QuotaWindow {
    /// Utilization as it should be read at `now`. A window past its reset is a
    /// fresh window — `0.0` — never the stale last value. This is bug #2's fix.
    pub fn effective(&self, now: OffsetDateTime) -> f64 {
        match self.reset {
            Some(reset) if now >= reset => 0.0,
            _ => self.utilization,
        }
    }

    /// The reset instant only while it is still in the future at `now`.
    pub fn live_reset(&self, now: OffsetDateTime) -> Option<OffsetDateTime> {
        match self.reset {
            Some(reset) if now < reset => Some(reset),
            _ => None,
        }
    }
}

/// One of the three rate-limit windows a [`Quota`] tracks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaBucket {
    /// The shared 5-hour session window (`unified-5h`).
    FiveHour,
    /// The shared weekly window (`unified-7d`).
    SevenDay,
    /// The model-scoped weekly window (`unified-7d_oi`, the Fable weekly).
    SevenDayOi,
}

/// Where the deadline a standing Codex refusal named for itself stands at a given instant
/// ([`Quota::codex_refusal_deadline`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalDeadline {
    /// No refusal stands, or the one that stands named no deadline.
    Unnamed,
    /// The named deadline is still ahead.
    Ahead(OffsetDateTime),
    /// The named deadline has passed, and no probe has read the refusal over yet.
    Passed,
}

/// All quota dimensions tracked for one account.
#[derive(Debug, Clone, Default)]
pub struct Quota {
    /// Shared 5-hour session bucket (`unified-5h`).
    pub five_hour: Option<QuotaWindow>,
    /// The usage endpoint's last word on the 5h window was an explicit `null`: this
    /// plan has no 5-hour window (an Ultra org, measured 2026-11-01), which is not
    /// the same fact as "unread". Set only by [`Self::apply_usage_at`] on
    /// [`crate::probe::WindowReading::PlanHasNone`], which also clears
    /// [`Self::five_hour`]; cleared by any 5h reading, from a probe or a header. The
    /// warmer skips an account with it set (`Manager::warm_targets`).
    pub five_hour_absent_on_plan: bool,
    /// Shared weekly bucket (`unified-7d`).
    pub seven_day: Option<QuotaWindow>,
    /// Model-scoped weekly bucket (`unified-7d_oi`, the Fable weekly on current
    /// plans). Gates Fable routing: a Fable-targeted request skips any account
    /// whose bucket is at/over threshold (see `model_weekly_exhausted` and the
    /// `is_fable` branch of `Manager::eligible`). Non-Fable routing ignores it.
    pub seven_day_oi: Option<QuotaWindow>,
    /// Last reported unified status: `allowed` | `allowed_warning` | `rejected`.
    /// A Codex account has no such header: its value comes from the response's HTTP
    /// status instead (see [`Quota::update_from_codex_refusal_at`]).
    pub status: Option<String>,
    // Standard API-key rate limits (present on non-OAuth accounts).
    pub tokens_limit: Option<i64>,
    pub tokens_remaining: Option<i64>,
    pub requests_limit: Option<i64>,
    pub requests_remaining: Option<i64>,
    /// Reset for the standard token/request limits.
    pub standard_reset: Option<OffsetDateTime>,
    /// Real, already-billed pay-as-you-go overage in USD, learned from a probe's
    /// `extra_usage` field (see [`crate::probe::Usage::extra_usage_usd`]). `None`
    /// means "never read", not "zero" — this display today falls back to
    /// [`crate::pricing`]'s synthetic token-counted estimate, and a fabricated
    /// `0.0` here would read as "no overage" instead of "we don't know yet".
    pub extra_usage_usd: Option<f64>,
    /// When a quota reading was last taken, from EITHER source: a successful
    /// usage probe ([`Self::apply_usage`]) or a response that carried rate-limit
    /// headers ([`Self::update_from_headers`], or [`Self::update_from_codex_refusal_at`]
    /// for a Codex account). `None` means nothing has been read since this process
    /// started.
    ///
    /// The probe's own clock (`AccountRuntime::last_probe_ms`) cannot answer "is
    /// this reading too old to trust": a busy account's windows move from headers
    /// between probes, and a failed probe stamps `last_probe_ms` while reading
    /// nothing, so an idle account with a broken probe shows frozen windows next
    /// to a recent probe time. This is display data only; nothing in selection
    /// reads it. It crosses the wire as `quotaUpdatedAtMs`.
    pub updated_at: Option<OffsetDateTime>,
    /// What a Codex account's readings said beyond its windows' utilization: which windows
    /// its plan has and how long they are, the refusal's kind, its spend control and its
    /// credits. See [`CodexQuotaFacts`]. Only the Codex readers write it
    /// ([`Self::update_from_codex_refusal_at`], and [`Self::apply_usage`] with a Codex probe's
    /// reading), so on an Anthropic account it stays the default.
    pub codex: CodexQuotaFacts,
    /// Anthropic's banked resets from the last successful Anthropic usage read, grant ids
    /// included (the claim names one). `None`: never read, or the last read carried no
    /// block. An ineligible answer is kept as one: unknown, not zero.
    pub claude_resets: Option<crate::claude_resets::ClaudeResetBank>,
}

/// A generic view over "case-insensitive header name → value" so the same
/// parsing works for `reqwest`'s [`reqwest::header::HeaderMap`] and for a plain
/// map in tests.
pub trait HeaderView {
    fn get_str(&self, name: &str) -> Option<&str>;
}

impl HeaderView for reqwest::header::HeaderMap {
    fn get_str(&self, name: &str) -> Option<&str> {
        self.get(name).and_then(|v| v.to_str().ok())
    }
}

impl Quota {
    /// Merge in whatever rate-limit headers are present on a response, leaving
    /// windows that were not reported this time untouched.
    ///
    /// Returns whether this response actually reported the **5-hour** window. That
    /// is the one dimension a caller needs to distinguish from silence: a response
    /// carrying the header is first-hand evidence about the account's session
    /// window, while a response without it (an error page, a non-Anthropic
    /// upstream) says nothing at all — and the two must never be confused for the
    /// keep-warm gate. See [`crate::manager::AccountRuntime::quota_known`].
    ///
    /// [`Self::updated_at`] is stamped on a different test than the return value:
    /// it moves when this response carried ANY quota header that was read (a
    /// window's utilization, the `7d_oi` allowed verdict, the unified status, or
    /// an API-key token or request limit). A `false` return therefore still counts
    /// as a fresh reading when, say, only the weekly window came back: that window
    /// is first-hand evidence read just now, and an API-key account never reports
    /// a 5h window at all, so keying on the return value would leave it reading
    /// as stale forever. A response with none of these headers (an error page, a
    /// non-Anthropic upstream) read nothing and leaves the stamp alone.
    #[must_use]
    pub fn update_from_headers(&mut self, headers: &impl HeaderView) -> bool {
        self.update_from_headers_at(headers, OffsetDateTime::now_utc())
    }

    /// Fold this reading at an explicit observation time.
    #[must_use]
    pub fn update_from_headers_at(
        &mut self,
        headers: &impl HeaderView,
        now: OffsetDateTime,
    ) -> bool {
        let mut read_five_hour = false;
        let mut read_any = false;
        if let Some(util) = get_parsed::<f64>(headers, "anthropic-ratelimit-unified-5h-utilization")
            .filter(|v| v.is_finite())
        {
            let reported = get_reset(headers, "anthropic-ratelimit-unified-5h-reset");
            let reset = resolve_reset(
                reported,
                self.five_hour.and_then(|w| w.reset),
                now,
                FIVE_HOUR,
            );
            self.five_hour = Some(QuotaWindow {
                utilization: util,
                reset,
            });
            // A 5h header is first-hand evidence the plan has the window.
            self.five_hour_absent_on_plan = false;
            read_five_hour = true;
            read_any = true;
        }
        if let Some(util) = get_parsed::<f64>(headers, "anthropic-ratelimit-unified-7d-utilization")
            .filter(|v| v.is_finite())
        {
            let reported = get_reset(headers, "anthropic-ratelimit-unified-7d-reset");
            let reset = resolve_reset(
                reported,
                self.seven_day.and_then(|w| w.reset),
                now,
                SEVEN_DAY,
            );
            self.seven_day = Some(QuotaWindow {
                utilization: util,
                reset,
            });
            read_any = true;
        }
        if let Some(util) =
            get_parsed::<f64>(headers, "anthropic-ratelimit-unified-7d_oi-utilization")
                .filter(|v| v.is_finite())
        {
            let reported = get_reset(headers, "anthropic-ratelimit-unified-7d_oi-reset");
            let reset = resolve_reset(
                reported,
                self.seven_day_oi.and_then(|w| w.reset),
                now,
                SEVEN_DAY,
            );
            self.seven_day_oi = Some(QuotaWindow {
                utilization: util,
                reset,
            });
            read_any = true;
        } else if scope_is_allowed(headers, "anthropic-ratelimit-unified-7d_oi-status")
            && self.seven_day_oi.is_some_and(|w| w.utilization >= 1.0)
        {
            // An `allowed` verdict for this scope carrying no utilization number is
            // still first-hand evidence, and it is how a rejection recorded by
            // [`Self::reject_model_weekly`] recovers before its reset. Overwrite the
            // window with allowed evidence rather than dropping it: a dropped window
            // loses its `reset`, which is what `account_gate` turns into the
            // "frees at" the exhausted-fleet response reports.
            let reported = get_reset(headers, "anthropic-ratelimit-unified-7d_oi-reset");
            self.seven_day_oi = Some(QuotaWindow {
                utilization: 0.0,
                reset: resolve_reset(
                    reported,
                    self.seven_day_oi.and_then(|w| w.reset),
                    now,
                    SEVEN_DAY,
                ),
            });
            read_any = true;
        }

        if let Some(status) = headers.get_str("anthropic-ratelimit-unified-status") {
            read_any = true;
            // The aggregate `rejected` is a terminal account gate
            // (`Manager::account_terminal_gate`), so it must not be stored when the
            // sub-scope headers say the rejection is model-scoped: the account still
            // serves every other model class, and the scope is already recorded in
            // `seven_day_oi`. Any other rejection — including one no sub-scope
            // explains — still applies account-wide. See issue #178.
            let model_scoped = matches!(
                quota_rejections(headers).as_slice(),
                [UnifiedRejectionKind::FableWeekly]
            );
            if !model_scoped {
                self.status = Some(status.to_string());
            }
        }

        if let Some(v) = get_parsed::<i64>(headers, "anthropic-ratelimit-tokens-limit") {
            self.tokens_limit = Some(v);
            read_any = true;
        }
        if let Some(v) = get_parsed::<i64>(headers, "anthropic-ratelimit-tokens-remaining") {
            self.tokens_remaining = Some(v);
            read_any = true;
        }
        if let Some(v) = get_parsed::<i64>(headers, "anthropic-ratelimit-requests-limit") {
            self.requests_limit = Some(v);
            read_any = true;
        }
        if let Some(v) = get_parsed::<i64>(headers, "anthropic-ratelimit-requests-remaining") {
            self.requests_remaining = Some(v);
            read_any = true;
        }
        if let Some(reset) = get_reset(headers, "anthropic-ratelimit-tokens-reset")
            .or_else(|| get_reset(headers, "anthropic-ratelimit-requests-reset"))
        {
            self.standard_reset = Some(reset);
            read_any = true;
        }
        if read_any {
            self.updated_at = Some(now);
        }
        read_five_hour
    }

    /// Is this account at/over `threshold` on any gating dimension, evaluated
    /// live at `now`? Only the shared 5-hour and weekly buckets gate rotation
    /// (plus standard API-key limits); the model-scoped `7d_oi` bucket does not.
    pub fn is_near(&self, threshold: f64, now: OffsetDateTime) -> bool {
        if let Some(w) = self.five_hour
            && w.effective(now) >= threshold
        {
            return true;
        }
        if let Some(w) = self.seven_day
            && w.effective(now) >= threshold
        {
            return true;
        }
        self.standard_limited(threshold, now)
    }

    /// Are the standard (API-key) token or request limits at/over `threshold` at `now`?
    /// One answer for [`Self::is_near`] and `Manager::account_gate`, so the gate and
    /// eligibility agree. OAuth accounts carry every standard field `None`, so this is
    /// always `false` for them.
    ///
    /// Standard limits have no live-past-reset path of their own, so once
    /// `standard_reset` has passed the upstream limit has refreshed and a spent count
    /// must NOT keep gating — otherwise a spent standard window pins the account out of
    /// rotation forever (there is no OAuth probe for API-key accounts to re-read it).
    /// Only gate while the window is still unexpired.
    pub fn standard_limited(&self, threshold: f64, now: OffsetDateTime) -> bool {
        if self.standard_reset.is_some_and(|r| now >= r) {
            return false;
        }
        let near = |limit: Option<i64>, remaining: Option<i64>| {
            matches!(
                (limit, remaining),
                (Some(l), Some(r)) if l > 0 && 1.0 - (r as f64 / l as f64) >= threshold
            )
        };
        near(self.tokens_limit, self.tokens_remaining)
            || near(self.requests_limit, self.requests_remaining)
    }

    /// Is the model-scoped weekly bucket (`7d_oi`, the Fable weekly) at/over
    /// `threshold`, evaluated live at `now`? Unlike [`Self::is_near`] — which
    /// deliberately ignores `7d_oi` so this bucket never gates non-Fable traffic —
    /// this is consulted ONLY for Fable requests by the manager. Reuses
    /// [`QuotaWindow::effective`], so a past-reset bucket reads a fresh `0.0`.
    pub fn model_weekly_exhausted(&self, threshold: f64, now: OffsetDateTime) -> bool {
        self.seven_day_oi
            .is_some_and(|w| w.effective(now) >= threshold)
    }

    /// Has upstream ACTUALLY rejected on the model-scoped weekly (`7d_oi`)
    /// bucket — as opposed to merely predicted it via `switchThreshold`? The
    /// only writer of a `1.0` utilization into `seven_day_oi` is
    /// [`Self::reject_model_weekly`], fired on a real upstream rejection; a
    /// live fleet's own reported `7d_oi` has been observed sitting at
    /// 0.95-0.98 while upstream keeps answering 200s, so `>= threshold` (used
    /// by [`Self::model_weekly_exhausted`]) is a PREDICTION and this is the
    /// AUTHORITY. Reuses [`QuotaWindow::effective`], so a past-reset rejection
    /// reads fresh as `0.0` (not rejected) once its window rolls over.
    pub fn model_weekly_rejected(&self, now: OffsetDateTime) -> bool {
        self.seven_day_oi.is_some_and(|w| w.effective(now) >= 1.0)
    }

    /// Record a model-scoped weekly rejection — upstream answered `rejected` on
    /// `7d_oi` while both shared scopes stayed allowed (issue #178) — by writing
    /// full utilization into the window the selector ALREADY gates Fable on. No
    /// separate deadline is kept: [`Self::model_weekly_exhausted`] reads this
    /// window live, so the rejection self-frees at its reset, and every existing
    /// path that learns a `7d_oi` utilization or an `allowed` verdict overwrites
    /// it. The reset comes from the matching header when present and otherwise
    /// from [`resolve_reset`], which inherits the known reset and, failing that,
    /// bounds the gate at one window — the account is never benched forever, and
    /// the shared windows are untouched so Opus and Sonnet stay eligible.
    pub fn reject_model_weekly(&mut self, headers: &impl HeaderView) {
        self.reject_model_weekly_at(headers, OffsetDateTime::now_utc());
    }

    /// Fold this reading at an explicit observation time.
    pub fn reject_model_weekly_at(&mut self, headers: &impl HeaderView, now: OffsetDateTime) {
        let reported = get_reset(headers, "anthropic-ratelimit-unified-7d_oi-reset");
        self.seven_day_oi = Some(QuotaWindow {
            utilization: 1.0,
            reset: resolve_reset(
                reported,
                self.seven_day_oi.and_then(|w| w.reset),
                now,
                SEVEN_DAY,
            ),
        });
    }

    /// Highest utilization across the dimensions that govern this request at `now`
    /// (shared 5-hour + weekly; the Fable weekly `7d_oi` too when `is_fable`), read
    /// live via [`QuotaWindow::effective`] so a past-reset window contributes a
    /// fresh `0.0`. A missing window contributes `0.0`. Used by
    /// [`crate::manager::Manager::select_revalidation`] to pick the least-utilized
    /// revalidation target — the account most likely to have stale headroom.
    /// Deliberately excludes the standard (API-key) token/request limits: OAuth
    /// accounts carry them `None`, and they are not trivially a single fraction.
    pub fn max_utilization(&self, now: OffsetDateTime, is_fable: bool) -> f64 {
        let mut max = 0.0f64;
        if let Some(w) = self.five_hour {
            max = max.max(w.effective(now));
        }
        if let Some(w) = self.seven_day {
            max = max.max(w.effective(now));
        }
        if is_fable && let Some(w) = self.seven_day_oi {
            max = max.max(w.effective(now));
        }
        max
    }

    /// Reset of the weekly bucket that governs rotation ordering, but only while
    /// it is still in the future at `now`. Used to spend the soonest-resetting
    /// quota first; `None` sorts an account first (unknown quota → probe it).
    pub fn governing_weekly_reset(&self, now: OffsetDateTime) -> Option<OffsetDateTime> {
        self.seven_day.and_then(|w| w.live_reset(now))
    }

    /// When a `rejected` unified status will clear on its own, if that can be known: the
    /// 5-hour window's reset instant, when the 5-hour window is the one carrying the
    /// rejection (its stored utilization is at least the weekly's). `None` when the account
    /// is not rejected, the weekly window is the fuller one (its reset is days away, never
    /// inside a cache window), or no reset was reported.
    ///
    /// Why this is knowable at all: `rejected` is upstream's verdict on a WINDOW, and
    /// [`Self::apply_usage`] drops it when that window rolls (the next probe reads a
    /// utilization that fell). So a 5h rejection is a timer with a deadline the headers
    /// already gave us, even though it carries no `retry-after`.
    pub fn five_hour_rejection_reset(&self) -> Option<OffsetDateTime> {
        if self.status.as_deref() != Some("rejected") {
            return None;
        }
        let five_hour = self.five_hour?;
        let weekly_util = self.seven_day.map_or(0.0, |w| w.utilization);
        if five_hour.utilization < weekly_util {
            return None;
        }
        five_hour.reset
    }

    /// `bucket`'s utilization as it reads at `now` ([`QuotaWindow::effective`]: a window
    /// past its reset reads a fresh `0.0`). `None` when the window was never read.
    pub fn effective(&self, bucket: QuotaBucket, now: OffsetDateTime) -> Option<f64> {
        self.window(bucket).map(|w| w.effective(now))
    }

    /// `bucket`'s reset while it is still ahead of `now` ([`QuotaWindow::live_reset`]).
    pub fn live_reset(&self, bucket: QuotaBucket, now: OffsetDateTime) -> Option<OffsetDateTime> {
        self.window(bucket).and_then(|w| w.live_reset(now))
    }

    const fn window(&self, bucket: QuotaBucket) -> Option<QuotaWindow> {
        match bucket {
            QuotaBucket::FiveHour => self.five_hour,
            QuotaBucket::SevenDay => self.seven_day,
            QuotaBucket::SevenDayOi => self.seven_day_oi,
        }
    }

    /// A quota as if these readings had been taken: the windows, and for a Codex row the
    /// windows its plan reported. Builds the fake rows of `tcr demo`, which reads no account.
    pub fn seeded(
        five_hour: Option<QuotaWindow>,
        seven_day: Option<QuotaWindow>,
        probed_codex_windows: Option<CodexWindows>,
    ) -> Self {
        Self {
            five_hour,
            seven_day,
            codex: CodexQuotaFacts {
                probed_windows: probed_codex_windows,
                ..CodexQuotaFacts::default()
            },
            ..Self::default()
        }
    }

    /// Anthropic's banked resets while the last read says this account may spend them: what
    /// a claim picks a grant from and what the auto-spend looks for expiring grants in.
    /// `None` when never read, or the read answered ineligible.
    pub fn eligible_claude_resets(&self) -> Option<&crate::claude_resets::ClaudeResetBank> {
        self.claude_resets.as_ref().filter(|bank| bank.eligible)
    }

    /// A Codex account's banked reset credits while it holds at least one.
    pub fn spendable_codex_reset_credits(&self) -> Option<&tcr_status_wire::ResetCredits> {
        self.codex
            .reset_credits
            .as_ref()
            .filter(|credits| credits.available_count > 0)
    }

    /// Where the deadline the standing Codex refusal named for itself (the usage-limit body's
    /// reset, [`CodexQuotaRefusal::reset`]) stands at `now`.
    pub fn codex_refusal_deadline(&self, now: OffsetDateTime) -> RefusalDeadline {
        match self.codex.refusal_reset {
            None => RefusalDeadline::Unnamed,
            Some(reset) if reset > now => RefusalDeadline::Ahead(reset),
            Some(_) => RefusalDeadline::Passed,
        }
    }

    /// What the last Codex response that carried any window said: how many windows it
    /// carried, and the stored reading of each it carried, five-hour then weekly (`None` for
    /// one it did not carry, or one carried but since cleared). `None` before any response
    /// carried a window.
    pub fn codex_header_readings(&self) -> Option<(usize, [Option<QuotaWindow>; 2])> {
        let carried = self.codex.header_windows?;
        Some((
            usize::from(carried.five_hour.is_some()) + usize::from(carried.seven_day.is_some()),
            [
                carried.five_hour.and(self.five_hour),
                carried.seven_day.and(self.seven_day),
            ],
        ))
    }

    /// The provider facts this quota reports on a row of the status wire. An Anthropic row
    /// reports its banked resets (without grant ids) and nothing else; a Codex row reports
    /// which windows its plan has (the probe's once any probe answered, else the headers'),
    /// each one's length, the refusal's kind while that refusal stands, the spend control,
    /// the credits and the banked reset credits. The row's own facts (its login, its folder)
    /// are the caller's.
    pub fn wire_facts(&self, provider: crate::provider::Provider) -> crate::stats::CodexFacts {
        match provider {
            crate::provider::Provider::Anthropic => crate::stats::CodexFacts {
                banked_resets: self
                    .claude_resets
                    .as_ref()
                    .map(crate::claude_resets::ClaudeResetBank::wire),
                ..crate::stats::CodexFacts::default()
            },
            crate::provider::Provider::Codex => {
                let windows = self.codex.windows();
                crate::stats::CodexFacts {
                    quota_windows: windows.map(|w| w.wire_names()),
                    five_hour_window_minutes: windows
                        .and_then(|w| w.five_hour)
                        .and_then(|w| w.minutes),
                    seven_day_window_minutes: windows
                        .and_then(|w| w.seven_day)
                        .and_then(|w| w.minutes),
                    limit_reached_type: self.codex_limit_reached_type().map(str::to_string),
                    spend_control_reached: self.codex.spend_control_reached,
                    credits: self.codex.credits.clone(),
                    reset_credits: self.codex.reset_credits.clone(),
                    ..crate::stats::CodexFacts::default()
                }
            }
        }
    }

    /// Fold a background probe's usage into the tracked windows. Only fields the
    /// probe actually reported overwrite existing values. There are three cases,
    /// and the third is the one an earlier `unwrap_or(0.0)` got wrong:
    ///  - the probe reported a utilization → it overwrites the tracked one;
    ///  - it reported `None` and a prior window exists → the prior utilization
    ///    (and, via [`resolve_reset`], the prior reset) is left intact;
    ///  - it reported `None` and there is NO prior → the window stays **absent**.
    ///    "We could not read this" is not "this is at 0%", and a fabricated `0.0`
    ///    is byte-identical to a genuine one. See [`apply_bucket`].
    ///
    /// The 5h window has a fourth case: an explicit `"five_hour": null`
    /// ([`crate::probe::WindowReading::PlanHasNone`]) is a reading, not a gap, and
    /// clears the window and sets [`Self::five_hour_absent_on_plan`].
    pub fn apply_usage(&mut self, usage: &crate::probe::Usage) {
        self.apply_usage_at(usage, OffsetDateTime::now_utc());
    }

    /// Fold a successful probe on the caller's clock, including missing resets.
    pub fn apply_usage_at(&mut self, usage: &crate::probe::Usage, now: OffsetDateTime) {
        // MUST run before the merges below. It compares the probe's fresh
        // utilization against the one still stored, and those merges overwrite
        // exactly that value.
        self.drop_rejection_if_a_window_rolled(usage);

        match usage.five_hour {
            crate::probe::WindowReading::Unread => {}
            // The plan has no 5h window: whatever was tracked before is stale, and
            // keeping it would gate and warm a window that does not exist.
            crate::probe::WindowReading::PlanHasNone => {
                self.five_hour = None;
                self.five_hour_absent_on_plan = true;
            }
            crate::probe::WindowReading::Read(bucket) => {
                apply_bucket(&mut self.five_hour, Some(bucket), FIVE_HOUR, now);
                self.five_hour_absent_on_plan = false;
            }
        }
        apply_bucket(&mut self.seven_day, usage.seven_day, SEVEN_DAY, now);
        apply_bucket(&mut self.seven_day_oi, usage.seven_day_oi, SEVEN_DAY, now);
        // Same rule as the buckets above: only a reported value overwrites.
        // `None` (unread this time) leaves whatever was last learned in place,
        // rather than erasing it back to "unknown".
        if let Some(extra) = usage.extra_usage_usd {
            self.extra_usage_usd = Some(extra);
        }
        // Called only with a SUCCESSFUL probe's usage, so this is a reading even
        // when the endpoint reported no bucket: "we read this account's quota" is
        // about the read, the same rule `Manager::apply_usage` latches
        // `quota_known` on. A failed probe never reaches here.
        self.updated_at = Some(now);
        // Anthropic's banked resets: a successful Anthropic read replaces them, a block it did
        // not carry included. A Codex read carries `codex` facts and says nothing about them.
        if usage.codex.is_none() {
            self.claude_resets.clone_from(&usage.claude_resets);
        }
        // A Codex probe's facts (an Anthropic probe carries none). Its windows are the plan's
        // and outrank the headers' from now on (`CodexQuotaFacts::windows`); its spend control
        // is current; its credits stand until another reading reports them.
        if let Some(facts) = &usage.codex {
            self.codex.probed_windows = Some(facts.windows);
            self.codex.spend_control_reached = facts.spend_control_reached;
            // A successful read replaces this fact, including missing or malformed summaries.
            // Failed reads never reach apply_usage and retain the prior reading.
            self.codex.reset_credits.clone_from(&facts.reset_credits);
            if let Some(credits) = &facts.credits {
                self.codex.credits = Some(credits.clone());
            }
            // Before the kind's refresh below, so a lifted refusal takes no kind with it.
            self.lift_a_codex_refusal_the_probe_reads_allowed(facts);
            // Only while a refusal stands: the kind is present exactly as long as the refusal.
            // `drop_rejection_if_a_window_rolled` ran first, so a probe that sees the window
            // roll has already ended the refusal before its own kind could refresh it.
            if self.status.as_deref() == Some("rejected")
                && let Some(kind) = &facts.limit_reached_type
            {
                self.codex.limit_reached_type = Some(kind.clone());
            }
        }
    }

    /// End a Codex refusal that the usage probe reads as over: the backend says the account is
    /// allowed, not at its limit, with its spend control not reached and no refusal kind.
    ///
    /// Why: that is the backend's own verdict on the account. A spend cap or a credits refusal
    /// leaves the windows where they were, so [`Self::drop_rejection_if_a_window_rolled`] never
    /// fires for it, and without this no probe ever ended it. Every condition is needed: a
    /// probe that reads the account refused, at its limit, at its spend control, or carrying a
    /// refusal kind is still reporting the refusal, and a probe with no `rate_limit` carries no
    /// verdict at all.
    ///
    /// A wrong lift costs one refused request, which marks the account again: the same trade
    /// [`Self::drop_rejection_if_a_window_rolled`] makes. Runs after that window check, in
    /// [`Self::apply_usage`].
    fn lift_a_codex_refusal_the_probe_reads_allowed(
        &mut self,
        facts: &crate::codex_probe::CodexProbeFacts,
    ) {
        if self.status.as_deref() == Some("rejected")
            && facts.allowed == Some(true)
            && facts.limit_reached != Some(true)
            && !facts.spend_control_reached
            && facts.limit_reached_type.is_none()
        {
            self.status = None;
            self.codex.limit_reached_type = None;
            self.codex.refusal_reset = None;
        }
    }

    /// Drop a `rejected` status whose window has since rolled.
    ///
    /// Without this, a rejection outlives its cause and benches the account until
    /// the process restarts. [`Quota::update_from_headers`] is the only writer of
    /// `status` (with, for a Codex account, [`Quota::update_from_codex_refusal_at`]), and
    /// it reads a SERVED response's `anthropic-ratelimit-unified-status` (the Codex one,
    /// the response's HTTP status). But
    /// [`crate::manager::Manager::account_terminal_gate`] skips an account carrying
    /// `rejected`, so no further served response ever arrives to refresh it, and
    /// this probe path used to move the windows while leaving `status` alone.
    /// Nothing else cleared it: `Quota` is runtime state, never serialized, so only
    /// a restart, a keep-warm response (`warmupSeconds`, off by default) or a human
    /// ended a rejection. A refused Codex account is not kept from traffic that way: once
    /// the hold its refusal armed lapses, the pool pick, which never reads `status`, may send
    /// it a request, and upstream is the oracle, since a 2xx that carries a window reads it
    /// allowed and another refusal marks it again ([`Quota::update_from_codex_refusal_at`]). A
    /// probe that reads the account allowed ends the refusal too, the backend's own word
    /// ([`Self::lift_a_codex_refusal_the_probe_reads_allowed`], which runs after this).
    ///
    /// **The signal is a utilization that FELL.** A shared window's utilization only
    /// rises while that window is running, so a probed value strictly below the one
    /// still stored is upstream telling us a new window started. Deliberately NOT
    /// the reset instant: [`apply_bucket`] synthesizes a reset for a high
    /// utilization reported without one, so a "the reset moved later" test could
    /// fire on a value we invented rather than one upstream reported.
    ///
    /// Only the two SHARED windows are consulted. A Fable-only rejection never
    /// reaches `status` at all (see [`Quota::update_from_headers`]), so
    /// `seven_day_oi` cannot be the cause of one.
    ///
    /// `None` rather than `Some("allowed")`: a rolled window is evidence the old
    /// verdict is stale, not evidence of a new one. The two readers both test
    /// `== Some("rejected")`, so `None` un-gates without asserting anything we did
    /// not observe.
    ///
    /// Clearing wrongly is cheap and self-correcting. EVERY upstream response
    /// including a 429 is folded through [`Quota::update_from_headers`], or a Codex
    /// account's through [`Quota::update_from_codex_refusal_at`]
    /// (`proxy.rs`, immediately after the "upstream response" log line), so an
    /// account that really is still rejected is marked again on its very next
    /// attempt, at a cost of one request. Clearing too late is the old behaviour: a
    /// permanently benched account.
    fn drop_rejection_if_a_window_rolled(&mut self, usage: &crate::probe::Usage) {
        if self.status.as_deref() != Some("rejected") {
            return;
        }
        if window_rolled(self.five_hour, usage.five_hour.bucket())
            || window_rolled(self.seven_day, usage.seven_day)
        {
            self.status = None;
            // A Codex refusal's facts live exactly as long as the refusal.
            self.codex.limit_reached_type = None;
            self.codex.refusal_reset = None;
        }
    }
}

/// Did this window roll between the stored reading and the probed one?
///
/// True only when BOTH utilizations are known and the fresh one is strictly lower.
/// An unknown on either side is not evidence of a roll, so it answers `false` and
/// the rejection stands.
fn window_rolled(stored: Option<QuotaWindow>, probed: Option<crate::probe::UsageBucket>) -> bool {
    match (stored, probed.and_then(|b| b.utilization)) {
        (Some(stored), Some(probed)) => probed < stored.utilization,
        _ => false,
    }
}

/// Merge a probed [`crate::probe::UsageBucket`] into an existing window in place.
/// A probed utilization that carries no reset gets a bounded synthesized reset
/// (`window` ahead of now) via [`resolve_reset`], so it can never pin the account
/// out of rotation forever (bug #4/#5).
///
/// **Exception — a ZERO utilization never synthesizes a reset.** The synthesis
/// above exists to bound a *high* utilization reported without a reset; a bucket
/// at `0.0` gates nothing, so inventing a reset for it protects nothing and
/// actively asserts a window that no real usage ever started. That fabrication
/// was not cosmetic: the zero-spend probe runs every `quotaProbeSeconds`, so it
/// stamped every idle account with a future 5h reset, and
/// [`crate::manager::Manager::warm_targets`] reads exactly that signal
/// (`five_hour.live_reset(now).is_some()`) as "already warm — skip". Measured on
/// the live fleet 2026-07-31: 7 of 13 accounts carried a future 5h reset at
/// `0.00` utilization, and keep-warm had **0 eligible targets** as a result.
/// Leaving the reset `None` here is the truthful state — we have no evidence of
/// a live window — and it is invisible to routing, because
/// [`QuotaWindow::effective`] returns `0.0` for a zero bucket under both the
/// `Some(reset)` and `None` arms.
///
/// **Exception — an UNREADABLE utilization with no prior leaves the window
/// absent.** The bucket being present says only that the endpoint mentioned this
/// dimension; if its utilization did not parse (key missing, `null`, a
/// non-numeric string) and no prior reading exists, we have no measurement. This
/// used to `unwrap_or(0.0)` and commit `Some(QuotaWindow { utilization: 0.0 })`,
/// byte-identical to a genuine 0%. Three things read that fabrication differently
/// from an absent window — and because the bucket's reported reset rode along
/// with it (see below), the first two need only that reset:
///  - [`crate::manager::Manager::warm_targets`] reads
///    `five_hour.live_reset(now).is_some()` as "already warm — skip", so a
///    fabricated window carrying a reset suppresses keep-warm for an account we
///    have measured nothing about. This is the strongest of the three;
///  - [`Quota::governing_weekly_reset`] then answers `Some(reset)` rather than
///    `None` on `seven_day`, and rotation ordering sorts a `None` first
///    (`select.rs` maps it to `i128::MIN`, "unknown quota → probe it"), so the
///    fabrication moves the account out of the probe-me-first position;
///  - the rendered value is a confident `0%` bar instead of "unmeasured".
///
/// It does NOT bias utilization-based routing. [`Quota::max_utilization`] folds
/// only over the windows that are present, starting from `0.0`, so `Some(0.0)`
/// and an absent window are arithmetically identical there; and every gate built
/// on it is `effective(now) >= threshold`, which a zero never trips.
///
/// Absent is the truthful state and the wire already carries it: `snapshot.rs`
/// maps a `None` window to a `None` utilization, which survives `stats.rs` →
/// `status.rs` → `cli.rs` as JSON `null` and renders in TcrBar as a dashed bar
/// with an `unmeasured` pill.
///
/// The defect is **latent, not firing**. One read of the live payload showed a
/// numeric percentage on every `limits[]` entry it returned — one payload at one
/// moment, which is not a property of the endpoint.
///
/// The bucket's reported **reset is discarded** along with it. `QuotaWindow` has
/// no representation for "a reset with no utilization" (`utilization` is a plain
/// `f64`), and inventing one would be the same fabrication in the other field —
/// the `warm_targets` suppression above, which is precisely the failure the
/// zero-utilization exception was written for.
fn apply_bucket(
    window: &mut Option<QuotaWindow>,
    bucket: Option<crate::probe::UsageBucket>,
    win: Duration,
    now: OffsetDateTime,
) {
    let Some(bucket) = bucket else {
        return;
    };
    let prior = *window;
    let Some(utilization) = bucket.utilization.or_else(|| prior.map(|w| w.utilization)) else {
        // No reading, and nothing prior to keep. Leave the window absent rather
        // than fabricating a 0.0 — see the exception above. `prior` is `None` on
        // this arm by construction, so this destroys nothing.
        return;
    };
    let reported = bucket
        .reset_at_ms
        .and_then(|ms| OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000).ok());
    let reset = if reported.is_none() && utilization == 0.0 {
        // Nothing to gate — do not invent a window. See the exception above.
        None
    } else {
        resolve_reset(reported, prior.and_then(|w| w.reset), now, win)
    };
    *window = Some(QuotaWindow { utilization, reset });
}

/// Parse a trimmed header value into any `FromStr` target. The `f64` NaN/inf
/// guard is NOT applied here (the generic can't know `T` is a float) — the f64
/// call sites append `.filter(|v| v.is_finite())` themselves.
fn get_parsed<T: std::str::FromStr>(headers: &impl HeaderView, name: &str) -> Option<T> {
    headers.get_str(name).and_then(|s| s.trim().parse().ok())
}

/// Parse a reset header. Unified resets are epoch **seconds**; standard resets
/// may be an RFC3339 timestamp — try both.
fn get_reset(headers: &impl HeaderView, name: &str) -> Option<OffsetDateTime> {
    let raw = headers.get_str(name)?.trim();
    if let Ok(secs) = raw.parse::<i64>() {
        return OffsetDateTime::from_unix_timestamp(secs).ok();
    }
    OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc3339).ok()
}

/// One scope of the unified rate limit, as reported by the `unified-*-status`
/// header family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnifiedRejectionKind {
    /// The aggregate `unified-status`, when no sub-scope explains it.
    Overall,
    /// The shared 5-hour session window.
    FiveHour,
    /// The shared weekly window.
    SevenDay,
    /// The model-scoped weekly window (`7d_oi`, the Fable weekly).
    FableWeekly,
}

fn header_says(headers: &impl HeaderView, name: &str, expected: &str) -> bool {
    headers
        .get_str(name)
        .is_some_and(|s| s.eq_ignore_ascii_case(expected))
}

/// Does this scope report an allowed verdict (`allowed` or `allowed_warning`)?
fn scope_is_allowed(headers: &impl HeaderView, name: &str) -> bool {
    header_says(headers, name, "allowed") || header_says(headers, name, "allowed_warning")
}

/// Return each unified quota scope that reports durable exhaustion.
///
/// The aggregate `unified-status` is reported as [`UnifiedRejectionKind::Overall`]
/// only when no sub-scope explains it, because upstream sets the aggregate to
/// `rejected` whenever ANY scope is — so a Fable-only weekly rejection arrives
/// with `unified-status: rejected`, `5h: allowed`, `7d: allowed_warning` and
/// reads account-wide if the aggregate is taken at face value. That is issue
/// #178: one Fable 429 benched a whole account for every model.
///
/// Written by `YogevKr`; verified by hand against the header block in #178.
pub fn quota_rejections(headers: &impl HeaderView) -> Vec<UnifiedRejectionKind> {
    let status = |name: &str, expected: &str| header_says(headers, name, expected);
    let allowed = |name: &str| scope_is_allowed(headers, name);
    let five_rejected = status("anthropic-ratelimit-unified-5h-status", "rejected");
    let seven_rejected = status("anthropic-ratelimit-unified-7d-status", "rejected");
    let fable_rejected = status("anthropic-ratelimit-unified-7d_oi-status", "rejected");
    let shared_allowed = allowed("anthropic-ratelimit-unified-5h-status")
        && allowed("anthropic-ratelimit-unified-7d-status");
    let overall_rejected = status("anthropic-ratelimit-unified-status", "rejected")
        && !(five_rejected || seven_rejected || fable_rejected && shared_allowed);

    [
        (overall_rejected, UnifiedRejectionKind::Overall),
        (five_rejected, UnifiedRejectionKind::FiveHour),
        (seven_rejected, UnifiedRejectionKind::SevenDay),
        (fable_rejected, UnifiedRejectionKind::FableWeekly),
    ]
    .into_iter()
    .filter_map(|(rejected, kind)| rejected.then_some(kind))
    .collect()
}

/// One Codex usage window a reading carried. Which of tcr's buckets it fills is the field of
/// [`CodexWindows`] it sits in; this is how long it is.
///
/// The length is kept because it is a fact of the plan the bars cannot show: a weekly-length
/// slot can be seven days or thirty, and both fill the weekly bucket.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CodexWindow {
    /// The window's length in minutes, as the reading reported it: the headers'
    /// `window-minutes` as is, or the probe's `limit_window_seconds` divided by 60. `None` when
    /// the reading named no length; a length of zero or less is no length.
    pub minutes: Option<u32>,
}

/// Which usage windows one Codex reading carried, by tcr's bucket, and how long each is.
///
/// Which windows a plan has is a fact a reading carries, not something every account shares: a
/// ChatGPT Pro login reports its weekly window and no five-hour one (measured 2026-10-12 and
/// 2026-10-14), and a plan can report no window at all. So a bucket is present only when the
/// reading carried a window for it, by the no-data rule both readers apply: a placeholder slot
/// at zero `used-percent`, with no length and no reset, is not a window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CodexWindows {
    pub five_hour: Option<CodexWindow>,
    pub seven_day: Option<CodexWindow>,
}

impl CodexWindows {
    /// The windows present, as the status wire names them: `"fiveHour"` then `"sevenDay"`, each
    /// only when present. Empty when the reading carried none, which is a fact about the plan,
    /// not a gap in what tcr knows.
    pub fn wire_names(&self) -> Vec<String> {
        [(self.five_hour, "fiveHour"), (self.seven_day, "sevenDay")]
            .into_iter()
            .filter_map(|(window, name)| window.map(|_| name.to_string()))
            .collect()
    }
}

/// What a Codex account's readings said beyond its windows' utilization, kept on its
/// [`Quota`] for the status wire to report.
///
/// The windows are kept twice, once per reader, because the two can disagree and the probe
/// outranks the headers: Pro response headers may carry a five-hour slot the plan does not
/// have (at zero `used-percent` but with a length and a reset, which passes the no-data rule),
/// while the usage probe reports the plan's own windows. [`Self::windows`] says which wins.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CodexQuotaFacts {
    /// The windows the last successful usage probe carried. `Some` once any probe has answered,
    /// and `Some` of no window at all when the plan reported none.
    pub probed_windows: Option<CodexWindows>,
    /// The windows the last response that carried any window carried. A response that carried
    /// none leaves it alone: silence is not evidence.
    pub header_windows: Option<CodexWindows>,
    /// Why ChatGPT refused the account, verbatim: the refusal's
    /// `x-codex-rate-limit-reached-type`, or, while that refusal stands, the probe's
    /// `rate_limit_reached_type`. Read it through [`Quota::codex_limit_reached_type`], which
    /// answers only while the refusal stands.
    pub limit_reached_type: Option<String>,
    /// The usage-limit body's absolute Unix-seconds reset, ahead of window-header estimates.
    pub refusal_reset: Option<OffsetDateTime>,
    /// Whether the last successful probe said the account's spend control is reached.
    pub spend_control_reached: bool,
    /// The account's credits, from the last reading that reported them.
    pub credits: Option<tcr_status_wire::CodexCredits>,
    /// Banked resets from the last successful usage read; absent means unknown.
    pub reset_credits: Option<tcr_status_wire::ResetCredits>,
}

impl CodexQuotaFacts {
    /// The windows the account's plan has, as far as tcr knows: the probe's once any probe has
    /// answered, else the response headers'. `None` before any reading, which is unknown, not
    /// "no windows".
    ///
    /// Once any probe has answered, the probe's set of windows AND their lengths win over the
    /// headers', because the probe describes the plan: a Pro response's headers may carry a
    /// five-hour slot with a length the plan does not have, and reporting that length would
    /// contradict the windows the plan reports (see [`CodexQuotaFacts`]).
    pub fn windows(&self) -> Option<CodexWindows> {
        self.probed_windows.or(self.header_windows)
    }
}

impl Quota {
    /// Why ChatGPT refused this account, verbatim, and only while that refusal stands:
    /// `None` whenever `status` is not `rejected`, whatever is stored. Present exactly while the
    /// refusal stands, so a reader can rely on its presence; see
    /// [`CodexQuotaFacts::limit_reached_type`] for where it comes from.
    pub fn codex_limit_reached_type(&self) -> Option<&str> {
        self.codex
            .limit_reached_type
            .as_deref()
            .filter(|_| self.status.as_deref() == Some("rejected"))
    }
}

/// The longest window, in minutes, that is still tcr's FIVE-HOUR bucket; a longer window is
/// the weekly one.
///
/// Codex reports up to two windows, "primary" and "secondary" (`openai/codex`
/// `codex-api/src/rate_limits.rs:68-79`), and the natural reading of the two is 300 and 10 080
/// minutes: tcr's five-hour and weekly buckets. Measured 2026-10-12 on two Pro logins
/// (`scripts/codex-contract-probe.sh`, the usage endpoint), one exhausted (`used_percent` 100)
/// and one idle (0), the backend reports ONLY the weekly window, and in the PRIMARY slot. That
/// the response headers name the same two slots is read from the source, not measured: the
/// probe spends no inference. So the mapping is by LENGTH, with this cutoff, falling back to
/// position only when the length is absent: by position, that weekly number and its
/// days-away reset would land in `five_hour`.
pub const CODEX_FIVE_HOUR_CUTOFF_MINUTES: i64 = 600;

/// Every per-account Codex response header: the ten this module reads a quota fact from
/// (the two windows' `used-percent`, `window-minutes` and `reset-at`, the refusal's
/// `rate-limit-reached-type`, and the three credits headers: whether the account has credits,
/// whether they are unlimited, and their balance), then the one the Codex client reads and tcr
/// does not, a promotional message (`codex-api/src/rate_limits.rs`). The credits are read
/// BEFORE the strip, in [`Quota::update_from_codex_refusal_at`], which the proxy calls with the
/// upstream headers ahead of building the pooled answer. All eleven describe the SERVING
/// account, so `proxy::build_response` strips exactly these from a pooled Codex response
/// (`proxy::is_account_scoped`), as it strips `anthropic-ratelimit-*`: a pooled client would
/// otherwise be shown another account's credit balance, changing from request to request.
/// Named one by one and never matched by the `x-codex-` prefix: `x-codex-turn-state` shares
/// it, and that is the client's own sticky-routing token, replayed within a turn
/// (`core/src/client.rs:13,163,289-309`), which must reach it. The dynamic `x-{limit}-*`
/// families the same upstream file discovers by prefix (`parse_all_rate_limits`) are not
/// listed: their names are unmeasured.
pub const CODEX_QUOTA_HEADERS: [&str; 11] = [
    "x-codex-primary-used-percent",
    "x-codex-primary-window-minutes",
    "x-codex-primary-reset-at",
    "x-codex-secondary-used-percent",
    "x-codex-secondary-window-minutes",
    "x-codex-secondary-reset-at",
    CODEX_REACHED_TYPE_HEADER,
    CODEX_CREDITS_HAS_CREDITS_HEADER,
    CODEX_CREDITS_UNLIMITED_HEADER,
    CODEX_CREDITS_BALANCE_HEADER,
    "x-codex-promo-message",
];

/// Whether the serving Codex account has credits, `true` or `false`
/// (`codex-api/src/rate_limits.rs`). Read into [`CodexQuotaFacts::credits`], then stripped.
const CODEX_CREDITS_HAS_CREDITS_HEADER: &str = "x-codex-credits-has-credits";
/// Whether those credits are unlimited, `true` or `false`.
const CODEX_CREDITS_UNLIMITED_HEADER: &str = "x-codex-credits-unlimited";
/// The credits balance, kept verbatim (`12.50`).
const CODEX_CREDITS_BALANCE_HEADER: &str = "x-codex-credits-balance";

/// Sent on a Codex QUOTA refusal (`codex-api/src/rate_limits.rs`). Its presence is what tcr
/// reads as an independent quota signal beside the error body. Its value, verbatim, is the
/// refusal's kind ([`CodexQuotaFacts::limit_reached_type`]).
pub const CODEX_REACHED_TYPE_HEADER: &str = "x-codex-rate-limit-reached-type";

/// A quota refusal parsed once for both the quota snapshot and the selection hold.
/// Only a future body deadline or a full window is evidence for a long hold.
#[derive(Debug, Clone)]
pub struct CodexQuotaRefusal {
    pub reset: Option<OffsetDateTime>,
    pub limit_reached_type: Option<String>,
}

impl CodexQuotaRefusal {
    /// Recognize a Codex 429 from the body types/codes in `codex-api/src/api_bridge.rs`
    /// or the independent reached-type header, and read its recovery deadline once.
    pub fn parse(headers: &impl HeaderView, body: &[u8], now: OffsetDateTime) -> Option<Self> {
        let document = serde_json::from_slice::<serde_json::Value>(body).ok();
        let error = document.as_ref().and_then(|doc| doc.get("error"));
        Self::from_error(headers, error, now)
    }

    /// The same error envelope can arrive inside a successful Responses stream.
    /// Reuse the HTTP classifier and deadline rules without parsing the event twice.
    pub(crate) fn from_error(
        headers: &impl HeaderView,
        error: Option<&serde_json::Value>,
        now: OffsetDateTime,
    ) -> Option<Self> {
        let limit_reached_type = headers
            .get_str(CODEX_REACHED_TYPE_HEADER)
            .map(str::to_owned);
        if limit_reached_type.is_none() && !error.is_some_and(codex_quota_error) {
            return None;
        }
        let reset = error
            .and_then(|error| codex_error_reset(error, now))
            .or_else(|| {
                codex_hold_seconds(headers, now).and_then(|seconds| {
                    OffsetDateTime::from_unix_timestamp(
                        now.unix_timestamp().saturating_add(seconds),
                    )
                    .ok()
                })
            });
        Some(Self {
            reset,
            limit_reached_type,
        })
    }
}

fn codex_quota_error(error: &serde_json::Value) -> bool {
    matches!(
        error.get("type").and_then(serde_json::Value::as_str),
        Some("usage_limit_reached" | "usage_not_included" | "insufficient_quota")
    ) || matches!(
        error.get("code").and_then(serde_json::Value::as_str),
        Some(
            "insufficient_quota"
                | "usage_limit_reached"
                | "usage_not_included"
                | "credit_balance_exhausted"
                | "organization_spend_limit_exceeded"
                | "project_spend_limit_exceeded"
                | "organization_usage_limit_exceeded"
        )
    )
}

/// `resets_at` is Unix seconds, as Codex's `DateTime::from_timestamp(seconds, 0)` reads it.
/// Ignore elapsed deadlines, including clock-skewed ones, before trying relative seconds
/// and then header windows. Codex does not itself read `resets_in_seconds`.
fn codex_error_reset(error: &serde_json::Value, now: OffsetDateTime) -> Option<OffsetDateTime> {
    error
        .get("resets_at")
        .and_then(serde_json::Value::as_i64)
        .and_then(|seconds| OffsetDateTime::from_unix_timestamp(seconds).ok())
        .filter(|reset| *reset > now)
        .or_else(|| {
            error
                .get("resets_in_seconds")
                .and_then(serde_json::Value::as_i64)
                .filter(|seconds| *seconds > 0)
                .and_then(|seconds| now.checked_add(Duration::seconds(seconds)))
        })
}

impl Quota {
    /// Merge in Codex's per-response quota headers, leaving a window this response did not
    /// report untouched: the Codex twin of [`Self::update_from_headers`].
    ///
    /// `x-codex-{primary,secondary}-used-percent` is 0–100 and becomes the 0.0–1.0
    /// utilization every threshold comparison in the manager already speaks. `reset-at` is
    /// epoch seconds (`rate_limits.rs:195-217`) and goes through `resolve_reset` like every
    /// other window. `window-minutes` picks the bucket ([`CODEX_FIVE_HOUR_CUTOFF_MINUTES`]);
    /// only when it is absent does the slot decide, primary five-hour and secondary weekly.
    /// `seven_day_oi` is an Anthropic bucket and stays untouched.
    ///
    /// A slot is trusted only as far as it carries data, so one slot can neither fabricate a
    /// window nor overwrite the other's:
    ///
    /// - a `window-minutes` of zero or less is no length at all, never a five-hour one;
    /// - a slot at 0% with no length and no reset carries no data and is skipped, the rule
    ///   Codex's own reader applies (`parse_rate_limit_window` keeps a window only when its
    ///   percent is non-zero, its length non-zero, or its reset present). Read as a window it
    ///   would get a synthesized live reset, a confident 0% bar and a `true` here for a plan
    ///   that has no such window;
    /// - two slots that land in the same bucket keep the higher utilization and the later
    ///   reset, never the last one written. A 0% placeholder in the secondary slot must not
    ///   turn a weekly window read at 97% from the primary slot into 0%.
    ///
    /// Returns `true` when the FIVE-HOUR window was read, the contract of
    /// [`Self::update_from_headers`], so [`crate::manager::Manager::update_quota`] treats the
    /// two parsers alike. A plan that reports only its weekly window answers `false`.
    ///
    /// `http_status` stands in for Anthropic's `unified-status` header, which Codex has no
    /// equivalent of. A 2xx that carried a window, from either slot, means this account is
    /// being served (`allowed`). A 429 carrying `x-codex-rate-limit-reached-type`
    /// means it is not (`rejected`). The proxy parses body-only refusals once through
    /// [`CodexQuotaRefusal::parse`] and folds them with [`Self::update_from_codex_refusal_at`].
    /// The Codex usage probe
    /// ([`crate::codex_probe`]) is what clears it, as the Anthropic probe clears an Anthropic
    /// rejection: its reading is folded through [`Self::apply_usage`], where
    /// `drop_rejection_if_a_window_rolled` drops the `rejected` once a probed window's
    /// utilization falls below the one stored, which is the window rolling, and
    /// `lift_a_codex_refusal_the_probe_reads_allowed` drops it once the probe reads the
    /// account allowed, which is how a refusal no window explains ends. Anything else, a 429
    /// with no quota signal leaves `status` as it was.
    ///
    /// Beside the windows it records the facts the status wire reports
    /// ([`CodexQuotaFacts`]): which windows this response carried and how long each is, the
    /// refusal's `x-codex-rate-limit-reached-type` (set with `rejected`, cleared with it), and
    /// the credits, from any response that reported them.
    ///
    /// The headers are READ here and then stripped, as Anthropic's are: `proxy::build_response`
    /// drops [`CODEX_QUOTA_HEADERS`] from a pooled serve (`proxy::is_account_scoped`), since
    /// each pooled response describes whichever account served it and the client would show
    /// a usage picture that changes per request. A relayed (client-credential) response keeps
    /// them: they describe the client's own account.
    /// Fold headers and the refusal already parsed by the proxy together. A body-only
    /// refusal is the same rejection as a header refusal; its reset takes precedence.
    #[must_use]
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "retain the wall-clock adapter; production folds through the explicit-time delegate"
        )
    )]
    pub(crate) fn update_from_codex_refusal(
        &mut self,
        headers: &impl HeaderView,
        http_status: u16,
        refusal: Option<&CodexQuotaRefusal>,
    ) -> bool {
        self.update_from_codex_refusal_at(headers, http_status, refusal, OffsetDateTime::now_utc())
    }

    /// The response and its already-parsed refusal share one observation time.
    #[must_use]
    pub(crate) fn update_from_codex_refusal_at(
        &mut self,
        headers: &impl HeaderView,
        http_status: u16,
        refusal: Option<&CodexQuotaRefusal>,
        now: OffsetDateTime,
    ) -> bool {
        // This response's reading of each bucket, (utilization, reported reset, length in
        // minutes), merged across slots before anything is written: see the doc for why a
        // slot may not simply overwrite.
        let mut five_hour: Option<(f64, Option<OffsetDateTime>, Option<u32>)> = None;
        let mut weekly: Option<(f64, Option<OffsetDateTime>, Option<u32>)> = None;
        for (prefix, positional_is_weekly) in
            [("x-codex-primary", false), ("x-codex-secondary", true)]
        {
            let Some(used) = get_parsed::<f64>(headers, &format!("{prefix}-used-percent"))
                .filter(|v| v.is_finite())
            else {
                continue;
            };
            // Zero or less is no length.
            let minutes =
                get_parsed::<i64>(headers, &format!("{prefix}-window-minutes")).filter(|&m| m > 0);
            let reported = get_parsed::<i64>(headers, &format!("{prefix}-reset-at"))
                .and_then(|secs| OffsetDateTime::from_unix_timestamp(secs).ok());
            // No data, by Codex's own rule once a non-positive length counts as none.
            if used == 0.0 && minutes.is_none() && reported.is_none() {
                continue;
            }
            let bucket = match minutes {
                Some(m) if m > CODEX_FIVE_HOUR_CUTOFF_MINUTES => &mut weekly,
                Some(_) => &mut five_hour,
                None if positional_is_weekly => &mut weekly,
                None => &mut five_hour,
            };
            let utilization = used / 100.0;
            let length = minutes.and_then(|m| u32::try_from(m).ok());
            *bucket = Some(match *bucket {
                None => (utilization, reported, length),
                Some((other, other_reset, other_length)) => (
                    other.max(utilization),
                    other_reset.max(reported),
                    codex_kept_length((other, other_length), (utilization, length)),
                ),
            });
        }
        // Any window is evidence this response came from a served Codex account. Not
        // `read_five_hour`: on the measured plan the only window is the weekly one.
        let read_a_window = five_hour.is_some() || weekly.is_some();
        let read_five_hour = five_hour.is_some();
        // Which windows this response carried, and how long each is. Only a response that
        // carried one says anything about the plan's windows; silence leaves them alone.
        if read_a_window {
            self.codex.header_windows = Some(CodexWindows {
                five_hour: five_hour.map(|(_, _, minutes)| CodexWindow { minutes }),
                seven_day: weekly.map(|(_, _, minutes)| CodexWindow { minutes }),
            });
        }
        if let Some((utilization, reported, _)) = weekly {
            let reset = resolve_reset(
                reported,
                self.seven_day.and_then(|w| w.reset),
                now,
                SEVEN_DAY,
            );
            self.seven_day = Some(QuotaWindow { utilization, reset });
        }
        if let Some((utilization, reported, _)) = five_hour {
            let reset = resolve_reset(
                reported,
                self.five_hour.and_then(|w| w.reset),
                now,
                FIVE_HOUR,
            );
            self.five_hour = Some(QuotaWindow { utilization, reset });
        }
        let refused = refusal.is_some();
        // The refusal's kind is set in the statement that marks the refusal and cleared in the
        // one that ends it, so it is present exactly while the refusal stands.
        if let Some(refusal) = refusal {
            self.codex.refusal_reset = refusal.reset;
            self.status = Some("rejected".to_string());
            self.codex
                .limit_reached_type
                .clone_from(&refusal.limit_reached_type);
        } else if read_a_window && (200..300).contains(&http_status) {
            self.status = Some("allowed".to_string());
            self.codex.limit_reached_type = None;
            self.codex.refusal_reset = None;
        }
        if let Some(credits) = codex_credits_from_headers(headers) {
            self.codex.credits = Some(credits);
        }
        // `updated_at` moves on the rule `update_from_headers` stamps it by: this
        // response carried a Codex quota fact that was read, a window from either slot
        // or the refusal verdict, so it counts even when the return value is `false`
        // (the measured plan reports only its weekly window). A response with neither,
        // an error page or a 429 that is not the quota, read nothing and leaves it.
        if read_a_window || refused {
            self.updated_at = Some(now);
        }
        read_five_hour
    }
}

/// The credits a Codex response reported, or `None` when it reported none and the last
/// reading should stand.
///
/// `x-codex-credits-has-credits` is required, `true` or `false` in any case: without it the
/// response said nothing about credits. `x-codex-credits-unlimited` is `false` unless it says
/// `true`. `x-codex-credits-balance` is kept trimmed and otherwise verbatim (`"12.50"`), and an
/// empty one is no balance.
fn codex_credits_from_headers(headers: &impl HeaderView) -> Option<tcr_status_wire::CodexCredits> {
    let flag = |name: &str| {
        let raw = headers.get_str(name)?.trim();
        if raw.eq_ignore_ascii_case("true") {
            Some(true)
        } else if raw.eq_ignore_ascii_case("false") {
            Some(false)
        } else {
            None
        }
    };
    Some(tcr_status_wire::CodexCredits {
        has_credits: flag(CODEX_CREDITS_HAS_CREDITS_HEADER)?,
        unlimited: flag(CODEX_CREDITS_UNLIMITED_HEADER).unwrap_or(false),
        balance: headers
            .get_str(CODEX_CREDITS_BALANCE_HEADER)
            .map(str::trim)
            .filter(|balance| !balance.is_empty())
            .map(str::to_string),
    })
}

/// The length a bucket keeps when two Codex windows land in it, each given as (utilization,
/// length in minutes): the length of the window whose utilization the bucket keeps, which is the
/// fuller one, and the longer of the two on equal utilization.
///
/// One rule for both readers, the headers ([`Quota::update_from_codex_refusal_at`]) and the probe
/// ([`crate::codex_probe::usage_from_payload`]), so they report one account's lengths alike. A
/// utilization the reading did not carry is passed as `f64::NEG_INFINITY`, so the window that
/// did carry one is the one kept, as the bucket keeps its utilization.
pub(crate) fn codex_kept_length(
    first: (f64, Option<u32>),
    second: (f64, Option<u32>),
) -> Option<u32> {
    if first.0 > second.0 {
        first.1
    } else if second.0 > first.0 {
        second.1
    } else {
        first.1.max(second.1)
    }
}

/// Seconds until the latest future reset of a FULL Codex window. A partly used
/// window cannot explain a refusal (for example a spend cap), and an elapsed reset
/// cannot justify a zero hold. Without evidence, callers use a capped retry delay.
pub fn codex_hold_seconds(headers: &impl HeaderView, now: OffsetDateTime) -> Option<i64> {
    ["x-codex-primary", "x-codex-secondary"]
        .into_iter()
        .filter_map(|prefix| {
            let used = get_parsed::<f64>(headers, &format!("{prefix}-used-percent"))?;
            if !used.is_finite() || used < 100.0 {
                return None;
            }
            let reset = get_parsed::<i64>(headers, &format!("{prefix}-reset-at"))?;
            let at = OffsetDateTime::from_unix_timestamp(reset).ok()?;
            (at > now).then(|| reset.saturating_sub(now.unix_timestamp()))
        })
        .max()
}

#[cfg(test)]
mod tests;
