//! Per-account quota, learned passively from upstream response headers.
//!
//! This is where **bug #2 is designed out**. In the JS proxy a quota bar keeps
//! the last-learned utilization until the next request re-learns it, so a window
//! that has already passed its reset can read as still-full. Here a window stores
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
/// fresh (bugs #4/#5 and #8 from the JS proxy):
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
    if let Some(prior) = prior {
        if prior > now {
            return Some(prior);
        }
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

/// All quota dimensions tracked for one account.
#[derive(Debug, Clone, Default)]
pub struct Quota {
    /// Shared 5-hour session bucket (`unified-5h`).
    pub five_hour: Option<QuotaWindow>,
    /// Shared weekly bucket (`unified-7d`).
    pub seven_day: Option<QuotaWindow>,
    /// Model-scoped weekly bucket (`unified-7d_oi`, the Fable weekly on current
    /// plans). Gates Fable routing: a Fable-targeted request skips any account
    /// whose bucket is at/over threshold (see `model_weekly_exhausted` and the
    /// `is_fable` branch of `Manager::eligible`). Non-Fable routing ignores it.
    pub seven_day_oi: Option<QuotaWindow>,
    /// Last reported unified status: `allowed` | `allowed_warning` | `rejected`.
    /// A Codex account has no such header: its value comes from the response's HTTP
    /// status instead (see [`Quota::update_from_codex_headers`]).
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
    #[must_use]
    pub fn update_from_headers(&mut self, headers: &impl HeaderView) -> bool {
        let now = OffsetDateTime::now_utc();
        let mut read_five_hour = false;
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
            read_five_hour = true;
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
        }

        if let Some(status) = headers.get_str("anthropic-ratelimit-unified-status") {
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
        }
        if let Some(v) = get_parsed::<i64>(headers, "anthropic-ratelimit-tokens-remaining") {
            self.tokens_remaining = Some(v);
        }
        if let Some(v) = get_parsed::<i64>(headers, "anthropic-ratelimit-requests-limit") {
            self.requests_limit = Some(v);
        }
        if let Some(v) = get_parsed::<i64>(headers, "anthropic-ratelimit-requests-remaining") {
            self.requests_remaining = Some(v);
        }
        if let Some(reset) = get_reset(headers, "anthropic-ratelimit-tokens-reset")
            .or_else(|| get_reset(headers, "anthropic-ratelimit-requests-reset"))
        {
            self.standard_reset = Some(reset);
        }
        read_five_hour
    }

    /// Is this account at/over `threshold` on any gating dimension, evaluated
    /// live at `now`? Only the shared 5-hour and weekly buckets gate rotation
    /// (plus standard API-key limits); the model-scoped `7d_oi` bucket does not.
    pub fn is_near(&self, threshold: f64, now: OffsetDateTime) -> bool {
        if let Some(w) = self.five_hour {
            if w.effective(now) >= threshold {
                return true;
            }
        }
        if let Some(w) = self.seven_day {
            if w.effective(now) >= threshold {
                return true;
            }
        }
        // Standard (API-key) token/request limits have no live-past-reset path of
        // their own, so once `standard_reset` has passed the upstream limit has
        // refreshed and a spent count must NOT keep gating — otherwise a spent
        // standard window pins the account out of rotation forever (there is no
        // OAuth probe for API-key accounts to re-read it). Only gate while the
        // window is still unexpired.
        let standard_expired = self.standard_reset.is_some_and(|r| now >= r);
        if !standard_expired {
            if let (Some(limit), Some(remaining)) = (self.tokens_limit, self.tokens_remaining) {
                if limit > 0 && 1.0 - (remaining as f64 / limit as f64) >= threshold {
                    return true;
                }
            }
            if let (Some(limit), Some(remaining)) = (self.requests_limit, self.requests_remaining) {
                if limit > 0 && 1.0 - (remaining as f64 / limit as f64) >= threshold {
                    return true;
                }
            }
        }
        false
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
        let now = OffsetDateTime::now_utc();
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
        if is_fable {
            if let Some(w) = self.seven_day_oi {
                max = max.max(w.effective(now));
            }
        }
        max
    }

    /// Reset of the weekly bucket that governs rotation ordering, but only while
    /// it is still in the future at `now`. Used to spend the soonest-resetting
    /// quota first; `None` sorts an account first (unknown quota → probe it).
    pub fn governing_weekly_reset(&self, now: OffsetDateTime) -> Option<OffsetDateTime> {
        self.seven_day.and_then(|w| w.live_reset(now))
    }

    /// Fold a background probe's usage into the tracked windows. Only fields the
    /// probe actually reported overwrite existing values. There are three cases,
    /// and the third is the one an earlier `unwrap_or(0.0)` got wrong:
    ///  - the probe reported a utilization → it overwrites the tracked one;
    ///  - it reported `None` and a prior window exists → the prior utilization
    ///    (and, via [`resolve_reset`], the prior reset) is left intact — this is
    ///    the JS `applyUsageData` behaviour;
    ///  - it reported `None` and there is NO prior → the window stays **absent**.
    ///    "We could not read this" is not "this is at 0%", and a fabricated `0.0`
    ///    is byte-identical to a genuine one. See [`apply_bucket`].
    pub fn apply_usage(&mut self, usage: &crate::probe::Usage) {
        // MUST run before the merges below. It compares the probe's fresh
        // utilization against the one still stored, and those merges overwrite
        // exactly that value.
        self.drop_rejection_if_a_window_rolled(usage);

        apply_bucket(&mut self.five_hour, usage.five_hour, FIVE_HOUR);
        apply_bucket(&mut self.seven_day, usage.seven_day, SEVEN_DAY);
        apply_bucket(&mut self.seven_day_oi, usage.seven_day_oi, SEVEN_DAY);
        // Same rule as the buckets above: only a reported value overwrites.
        // `None` (unread this time) leaves whatever was last learned in place,
        // rather than erasing it back to "unknown".
        if let Some(extra) = usage.extra_usage_usd {
            self.extra_usage_usd = Some(extra);
        }
    }

    /// Drop a `rejected` status whose window has since rolled.
    ///
    /// Without this, a rejection outlives its cause and benches the account until
    /// the process restarts. [`Quota::update_from_headers`] is the only writer of
    /// `status` (with, for a Codex account, [`Quota::update_from_codex_headers`]), and
    /// it reads a SERVED response's `anthropic-ratelimit-unified-status` (the Codex one,
    /// the response's HTTP status). But
    /// [`crate::manager::Manager::account_terminal_gate`] skips an account carrying
    /// `rejected`, so no further served response ever arrives to refresh it, and
    /// this probe path used to move the windows while leaving `status` alone.
    /// Nothing else clears it: `Quota` is runtime state, never serialized, so only
    /// a restart, a keep-warm response (`warmupSeconds`, off by default) or a human
    /// ended a rejection.
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
    /// account's through [`Quota::update_from_codex_headers`]
    /// (`proxy.rs`, immediately after the "upstream response" log line), so an
    /// account that really is still rejected is marked again on its very next
    /// attempt, at a cost of one request. Clearing too late is the old behaviour: a
    /// permanently benched account.
    fn drop_rejection_if_a_window_rolled(&mut self, usage: &crate::probe::Usage) {
        if self.status.as_deref() != Some("rejected") {
            return;
        }
        if window_rolled(self.five_hour, usage.five_hour)
            || window_rolled(self.seven_day, usage.seven_day)
        {
            self.status = None;
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
) {
    let Some(bucket) = bucket else {
        return;
    };
    let now = OffsetDateTime::now_utc();
    let prior = *window;
    let Some(utilization) = bucket.utilization.or_else(|| prior.map(|w| w.utilization)) else {
        // No reading, and nothing prior to keep. Leave the window absent rather
        // than fabricating a 0.0 — see the exception above. `prior` is `None` on
        // this arm by construction, so this destroys nothing.
        return;
    };
    let reported = bucket
        .reset_at_ms
        .and_then(|ms| OffsetDateTime::from_unix_timestamp_nanos(ms as i128 * 1_000_000).ok());
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
/// Written by YogevKr; verified by hand against the header block in #178.
pub fn quota_rejections(headers: &impl HeaderView) -> Vec<UnifiedRejectionKind> {
    let status = |name: &str, expected: &str| header_says(headers, name, expected);
    let allowed = |name: &str| scope_is_allowed(headers, name);
    let five_rejected = status("anthropic-ratelimit-unified-5h-status", "rejected");
    let seven_rejected = status("anthropic-ratelimit-unified-7d-status", "rejected");
    let fable_rejected = status("anthropic-ratelimit-unified-7d_oi-status", "rejected");
    let shared_allowed = allowed("anthropic-ratelimit-unified-5h-status")
        && allowed("anthropic-ratelimit-unified-7d-status");
    let overall_rejected = status("anthropic-ratelimit-unified-status", "rejected")
        && !five_rejected
        && !seven_rejected
        && !(fable_rejected && shared_allowed);

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

/// Every per-account Codex response header: the seven this module reads a quota fact from
/// (the two windows' `used-percent`, `window-minutes` and `reset-at`, and the refusal's
/// `rate-limit-reached-type`), then the four the Codex client reads and tcr does not
/// (`codex-api/src/rate_limits.rs`): whether the account has credits, whether they are
/// unlimited, their balance, and a promotional message. All eleven describe the SERVING
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
    "x-codex-credits-has-credits",
    "x-codex-credits-unlimited",
    "x-codex-credits-balance",
    "x-codex-promo-message",
];

/// Sent on a Codex QUOTA refusal (`codex-api/src/rate_limits.rs`). Its presence is what tcr
/// reads as "this 429 is the quota", as against any other 429.
pub const CODEX_REACHED_TYPE_HEADER: &str = "x-codex-rate-limit-reached-type";

/// Is this Codex response a QUOTA refusal? A 429 that carries
/// [`CODEX_REACHED_TYPE_HEADER`] is; a 429 without it is some other refusal, which marks
/// nothing `rejected` and holds its account on `retry-after` alone, never on a window's
/// reset. One predicate for both readers, [`Quota::update_from_codex_headers`] and the
/// proxy's hold, so the two cannot disagree about which 429 is which.
pub fn is_codex_quota_refusal(headers: &impl HeaderView) -> bool {
    headers.get_str(CODEX_REACHED_TYPE_HEADER).is_some()
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
    /// ([`is_codex_quota_refusal`]) means it is not (`rejected`, which `select()` treats as a
    /// hard fail, so no 2xx follows to clear it). Nothing clears a Codex `rejected` yet: no
    /// Codex usage probe exists in this build, so it stands until the process restarts. Once
    /// one does, `drop_rejection_if_a_window_rolled` lifts it when a probe sees the window
    /// roll, as it does for Anthropic. Anything else, a 429 without that header included,
    /// leaves `status` as it was.
    ///
    /// The headers are READ here and then stripped, as Anthropic's are: `proxy::build_response`
    /// drops [`CODEX_QUOTA_HEADERS`] from a pooled serve (`proxy::is_account_scoped`), since
    /// each pooled response describes whichever account served it and the client would show
    /// a usage picture that changes per request. A relayed (client-credential) response keeps
    /// them: they describe the client's own account.
    #[must_use]
    pub fn update_from_codex_headers(
        &mut self,
        headers: &impl HeaderView,
        http_status: u16,
    ) -> bool {
        let now = OffsetDateTime::now_utc();
        // This response's reading of each bucket, (utilization, reported reset), merged
        // across slots before anything is written: see the doc for why a slot may not
        // simply overwrite.
        let mut five_hour: Option<(f64, Option<OffsetDateTime>)> = None;
        let mut weekly: Option<(f64, Option<OffsetDateTime>)> = None;
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
            *bucket = Some(match *bucket {
                None => (utilization, reported),
                Some((other, other_reset)) => (other.max(utilization), other_reset.max(reported)),
            });
        }
        // Any window is evidence this response came from a served Codex account. Not
        // `read_five_hour`: on the measured plan the only window is the weekly one.
        let read_a_window = five_hour.is_some() || weekly.is_some();
        let read_five_hour = five_hour.is_some();
        if let Some((utilization, reported)) = weekly {
            let reset = resolve_reset(
                reported,
                self.seven_day.and_then(|w| w.reset),
                now,
                SEVEN_DAY,
            );
            self.seven_day = Some(QuotaWindow { utilization, reset });
        }
        if let Some((utilization, reported)) = five_hour {
            let reset = resolve_reset(
                reported,
                self.five_hour.and_then(|w| w.reset),
                now,
                FIVE_HOUR,
            );
            self.five_hour = Some(QuotaWindow { utilization, reset });
        }
        if http_status == 429 && is_codex_quota_refusal(headers) {
            self.status = Some("rejected".to_string());
        } else if read_a_window && (200..300).contains(&http_status) {
            self.status = Some("allowed".to_string());
        }
        read_five_hour
    }
}

/// How long a Codex QUOTA refusal should hold the account, in seconds from `now`: until the
/// LATEST reset among the windows that are full (`used-percent >= 100`), since an account
/// whose windows are both spent serves again only when both have rolled; else the reset of
/// the fullest window; else `None`. Never negative: a reset already past is `0`.
///
/// Asked only for a 429 that IS a quota refusal ([`is_codex_quota_refusal`]); the proxy
/// holds any other 429 on `retry-after` or its default short hold alone, so a non-quota 429
/// that still carries window headers cannot bench its account for up to an hour. `None`
/// sends a refusal to that same fallback.
///
/// A `used-percent` that is not a finite number counts as `0`, the guard every f64 read in
/// this module takes (`get_parsed`). NaN compares false both ways, so a NaN in the first
/// slot would otherwise keep that slot against a real 100% in the second.
pub fn codex_hold_seconds(headers: &impl HeaderView, now: OffsetDateTime) -> Option<i64> {
    let mut latest_full: Option<i64> = None;
    let mut fullest: Option<(f64, i64)> = None;
    for prefix in ["x-codex-primary", "x-codex-secondary"] {
        let used = get_parsed::<f64>(headers, &format!("{prefix}-used-percent"))
            .filter(|v| v.is_finite())
            .unwrap_or(0.0);
        let Some(reset) = get_parsed::<i64>(headers, &format!("{prefix}-reset-at")) else {
            continue;
        };
        if used >= 100.0 {
            latest_full = Some(latest_full.map_or(reset, |other| other.max(reset)));
        }
        if fullest.is_none_or(|(u, _)| used > u) {
            fullest = Some((used, reset));
        }
    }
    latest_full
        .or(fullest.map(|(_, reset)| reset))
        .map(|reset| reset.saturating_sub(now.unix_timestamp()).max(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use time::Duration;

    /// A test header bag with case-insensitive lookup.
    struct TestHeaders(HashMap<String, String>);

    impl TestHeaders {
        fn new(pairs: &[(&str, &str)]) -> Self {
            Self(
                pairs
                    .iter()
                    .map(|(k, v)| (k.to_lowercase(), v.to_string()))
                    .collect(),
            )
        }

        /// Add one header to a bag a shared fixture built, for the test that
        /// differs from that fixture by a single line.
        fn push(&mut self, name: &str, value: &str) {
            self.0.insert(name.to_lowercase(), value.to_string());
        }
    }

    impl HeaderView for TestHeaders {
        fn get_str(&self, name: &str) -> Option<&str> {
            self.0.get(&name.to_lowercase()).map(String::as_str)
        }
    }

    #[test]
    fn parses_unified_utilization_and_reset() {
        let reset_secs = 1_893_456_000i64;
        let headers = TestHeaders::new(&[
            ("anthropic-ratelimit-unified-5h-utilization", "0.42"),
            (
                "anthropic-ratelimit-unified-5h-reset",
                &reset_secs.to_string(),
            ),
            ("anthropic-ratelimit-unified-7d-utilization", "0.10"),
            ("anthropic-ratelimit-unified-status", "allowed_warning"),
        ]);
        let mut quota = Quota::default();
        assert!(
            quota.update_from_headers(&headers),
            "a response carrying the unified 5h header reports the window"
        );

        let five = quota.five_hour.unwrap();
        assert_eq!(five.utilization, 0.42);
        assert_eq!(
            five.reset,
            Some(OffsetDateTime::from_unix_timestamp(reset_secs).unwrap())
        );
        assert_eq!(quota.seven_day.unwrap().utilization, 0.10);
        assert_eq!(quota.status.as_deref(), Some("allowed_warning"));
    }

    /// Finding #11: a non-finite utilization header (`nan`/`inf`) must be treated
    /// as absent, not parsed into a NaN that reads as under-quota (NaN >= threshold
    /// is always false, so a spent account would stay in rotation). A normal numeric
    /// header still parses.
    #[test]
    fn rejects_non_finite_utilization_headers() {
        for bad in ["nan", "inf", "-inf"] {
            let headers = TestHeaders::new(&[("anthropic-ratelimit-unified-5h-utilization", bad)]);
            assert_eq!(
                get_parsed::<f64>(&headers, "anthropic-ratelimit-unified-5h-utilization")
                    .filter(|v| v.is_finite()),
                None,
                "{bad:?} must parse to None"
            );
            let mut quota = Quota::default();
            assert!(
                !quota.update_from_headers(&headers),
                "{bad:?} is not a readable window, so it must not report one either"
            );
            assert!(
                quota.five_hour.is_none(),
                "{bad:?} utilization must not create a window"
            );
        }

        let headers = TestHeaders::new(&[("anthropic-ratelimit-unified-5h-utilization", "0.87")]);
        assert_eq!(
            get_parsed::<f64>(&headers, "anthropic-ratelimit-unified-5h-utilization")
                .filter(|v| v.is_finite()),
            Some(0.87)
        );
    }

    /// Bug #2, proven: a window whose reset is in the PAST reads as a fresh 0.0,
    /// never the stale last-learned utilization — computed live against `now`.
    #[test]
    fn expired_quota_window_reads_as_fresh_zero() {
        let now = OffsetDateTime::now_utc();
        let window = QuotaWindow {
            utilization: 0.95,
            reset: Some(now - Duration::hours(1)), // already reset an hour ago
        };

        // Live read past the reset is fresh, not the cached 0.95.
        assert_eq!(window.effective(now), 0.0);

        // And an account carrying only that expired window is NOT near quota,
        // so it stays eligible for selection instead of being wrongly skipped.
        let quota = Quota {
            five_hour: Some(window),
            ..Quota::default()
        };
        assert!(!quota.is_near(0.90, now));

        // Sanity: before the reset the same window still reads full.
        let before_reset = now - Duration::hours(2);
        assert_eq!(window.effective(before_reset), 0.95);
        assert!(quota.is_near(0.90, before_reset));
    }

    #[test]
    fn is_near_true_when_five_hour_over_threshold() {
        let now = OffsetDateTime::now_utc();
        let quota = Quota {
            five_hour: Some(QuotaWindow {
                utilization: 0.95,
                reset: Some(now + Duration::hours(1)),
            }),
            ..Quota::default()
        };
        assert!(quota.is_near(0.90, now));
    }

    #[test]
    fn governing_weekly_reset_hides_past_reset() {
        let now = OffsetDateTime::now_utc();
        let quota = Quota {
            seven_day: Some(QuotaWindow {
                utilization: 0.5,
                reset: Some(now - Duration::minutes(5)),
            }),
            ..Quota::default()
        };
        assert_eq!(quota.governing_weekly_reset(now), None);
    }

    /// Finding #4/#8: a high utilization learned WITHOUT a reset must not pin the
    /// account out of rotation forever. `update_from_headers` synthesizes a
    /// bounded reset, so the window gates now but reads fresh once the synthesized
    /// window (5h) has elapsed — clearable, never permanent.
    #[test]
    fn resetless_high_util_window_is_bounded_not_permanent() {
        let mut quota = Quota::default();
        // Utilization with NO reset header.
        let headers = TestHeaders::new(&[("anthropic-ratelimit-unified-5h-utilization", "0.95")]);
        assert!(quota.update_from_headers(&headers));

        let now = OffsetDateTime::now_utc();
        // A reset was synthesized in the future, so it gates right now …
        assert!(
            quota.is_near(0.90, now),
            "resetless high util gates immediately"
        );
        assert!(
            quota.five_hour.unwrap().reset.is_some(),
            "a bounded reset was synthesized"
        );
        // … but clears once the synthesized 5h window has elapsed — not forever.
        let after_window = now + Duration::hours(6);
        assert!(
            !quota.is_near(0.90, after_window),
            "resetless window must clear after its bounded lifetime, never pin forever"
        );
    }

    /// Build a quota that a served 429 left carrying `rejected`, with `util`
    /// already spent on the shared window named by `on_seven_day`.
    fn rejected_with(on_seven_day: bool, util: f64) -> Quota {
        let window = Some(QuotaWindow {
            utilization: util,
            reset: Some(OffsetDateTime::now_utc() + time::Duration::hours(3)),
        });
        Quota {
            five_hour: if on_seven_day { None } else { window },
            seven_day: if on_seven_day { window } else { None },
            status: Some("rejected".to_string()),
            ..Default::default()
        }
    }

    fn bucket(util: f64) -> Option<crate::probe::UsageBucket> {
        Some(crate::probe::UsageBucket {
            utilization: Some(util),
            reset_at_ms: None,
        })
    }

    /// The bug this exists to end: `rejected` outliving the window that caused it.
    ///
    /// `update_from_headers` is the only writer of `status` and it needs a SERVED
    /// response, but `account_terminal_gate` skips a rejected account, so no served
    /// response ever arrives. Before this, the probe moved the bars and left
    /// `status` alone, and the account stayed benched until a restart.
    #[test]
    fn probed_drop_in_utilization_clears_a_stale_rejection() {
        for on_seven_day in [false, true] {
            let mut quota = rejected_with(on_seven_day, 0.99);
            let usage = if on_seven_day {
                crate::probe::Usage {
                    seven_day: bucket(0.04),
                    ..Default::default()
                }
            } else {
                crate::probe::Usage {
                    five_hour: bucket(0.04),
                    ..Default::default()
                }
            };

            quota.apply_usage(&usage);

            assert_eq!(
                quota.status, None,
                "a utilization that FELL is upstream starting a new window, so the \
                 rejection recorded against the old one is stale (seven_day={on_seven_day})"
            );
        }
    }

    /// The other side of the same rule: a rejection that is still current must
    /// survive the probe, or every probe cadence would put a genuinely blocked
    /// account back into rotation to earn another 429.
    #[test]
    fn probe_leaves_a_live_rejection_alone() {
        // Utilization climbing within the same window.
        let mut climbing = rejected_with(false, 0.97);
        climbing.apply_usage(&crate::probe::Usage {
            five_hour: bucket(0.99),
            ..Default::default()
        });
        assert_eq!(
            climbing.status.as_deref(),
            Some("rejected"),
            "a RISING utilization is the same window still running"
        );

        // Identical reading: no evidence of anything.
        let mut flat = rejected_with(false, 0.99);
        flat.apply_usage(&crate::probe::Usage {
            five_hour: bucket(0.99),
            ..Default::default()
        });
        assert_eq!(
            flat.status.as_deref(),
            Some("rejected"),
            "an unchanged utilization is not evidence of a roll"
        );

        // The probe read nothing for this window.
        let mut silent = rejected_with(false, 0.99);
        silent.apply_usage(&crate::probe::Usage::default());
        assert_eq!(
            silent.status.as_deref(),
            Some("rejected"),
            "silence is not evidence of a roll"
        );

        // We never stored a prior reading, so there is nothing to compare against.
        let mut unknown = Quota {
            status: Some("rejected".to_string()),
            ..Default::default()
        };
        unknown.apply_usage(&crate::probe::Usage {
            five_hour: bucket(0.01),
            ..Default::default()
        });
        assert_eq!(
            unknown.status.as_deref(),
            Some("rejected"),
            "with no stored utilization a low reading proves nothing"
        );
    }

    /// `seven_day_oi` is the model-scoped (Fable) weekly. A Fable-only rejection
    /// never reaches `status` in the first place (`update_from_headers`), so a roll
    /// on that window is not evidence about an account-wide rejection and must not
    /// clear one.
    #[test]
    fn a_fable_weekly_roll_does_not_clear_an_account_wide_rejection() {
        let mut quota = Quota {
            seven_day_oi: Some(QuotaWindow {
                utilization: 0.99,
                reset: Some(OffsetDateTime::now_utc() + time::Duration::hours(3)),
            }),
            status: Some("rejected".to_string()),
            ..Default::default()
        };

        quota.apply_usage(&crate::probe::Usage {
            seven_day_oi: bucket(0.02),
            ..Default::default()
        });

        assert_eq!(
            quota.status.as_deref(),
            Some("rejected"),
            "the model-scoped weekly cannot be the cause of an account-wide rejection"
        );
    }

    /// The clear is scoped to `rejected`. `allowed_warning` carries the near-limit
    /// signal the TUI colours on, and a rolled window must not silently erase it.
    #[test]
    fn a_rolled_window_does_not_erase_a_non_rejected_status() {
        let mut quota = Quota {
            status: Some("allowed_warning".to_string()),
            ..rejected_with(false, 0.99)
        };

        quota.apply_usage(&crate::probe::Usage {
            five_hour: bucket(0.01),
            ..Default::default()
        });

        assert_eq!(
            quota.status.as_deref(),
            Some("allowed_warning"),
            "only a rejection is dropped here"
        );
    }

    /// The zero-spend probe must NOT fabricate a 5h window on an idle account.
    ///
    /// Measured live 2026-07-31: every probe sweep stamped idle accounts with a
    /// reset exactly `now + 5h`, and `warm_targets` reads a live 5h reset as
    /// "already warm", so keep-warm had 0 eligible targets of 13 while 7
    /// accounts carried a future reset at 0.00 utilization. The probe starts no
    /// window — only a served request does — so a zero bucket must leave the
    /// reset `None`.
    #[test]
    fn probed_zero_bucket_does_not_fabricate_a_window() {
        let mut quota = Quota::default();
        quota.apply_usage(&crate::probe::Usage {
            five_hour: Some(crate::probe::UsageBucket {
                utilization: Some(0.0),
                reset_at_ms: None,
            }),
            ..Default::default()
        });

        let now = OffsetDateTime::now_utc();
        let window = quota.five_hour.expect("the bucket was applied");
        assert_eq!(
            window.reset, None,
            "a zero-utilization probe must not synthesize a reset"
        );
        assert_eq!(
            window.live_reset(now),
            None,
            "and must therefore not read as a live window to warm_targets"
        );
    }

    /// The guard above must not weaken bug #4/#5 on the path it was written for:
    /// a NON-zero probed utilization with no reset still gets a bounded window,
    /// so it gates now and clears later rather than pinning the account forever.
    #[test]
    fn probed_nonzero_bucket_without_reset_still_gets_a_bounded_window() {
        let mut quota = Quota::default();
        quota.apply_usage(&crate::probe::Usage {
            five_hour: Some(crate::probe::UsageBucket {
                utilization: Some(0.95),
                reset_at_ms: None,
            }),
            ..Default::default()
        });

        let now = OffsetDateTime::now_utc();
        assert!(
            quota.five_hour.expect("applied").reset.is_some(),
            "a resetless HIGH utilization still needs its bounded reset"
        );
        assert!(quota.is_near(0.90, now), "and gates immediately");
        assert!(
            !quota.is_near(0.90, now + Duration::hours(6)),
            "clearing after one bounded window, never pinning forever"
        );
    }

    /// A bucket the endpoint mentions but whose utilization does not parse, with
    /// no prior reading, must stay ABSENT — not be committed as a fabricated
    /// `0.0`. A fabricated zero carries the bucket's reset with it, which reads to
    /// `warm_targets` as "already warm — skip" and to `governing_weekly_reset` as
    /// a known window; and it renders as a confident `0%` bar. Absent reaches the
    /// wire as `null` and renders as "unmeasured".
    #[test]
    fn unreadable_utilization_with_no_prior_stays_absent() {
        let mut quota = Quota::default();
        quota.apply_usage(&crate::probe::Usage {
            five_hour: Some(crate::probe::UsageBucket {
                utilization: None,
                reset_at_ms: Some(crate::now_ms() + 3_600_000),
            }),
            ..Default::default()
        });

        assert!(
            quota.five_hour.is_none(),
            "an unreadable utilization with no prior must not fabricate a window"
        );
        // And the reported reset goes with it: a reset with no measurement behind
        // it would read as "already warm" to `warm_targets` and suppress keep-warm.
        let now = OffsetDateTime::now_utc();
        assert_eq!(
            quota.five_hour.and_then(|w| w.live_reset(now)),
            None,
            "no window means no live reset to suppress keep-warm with"
        );
        // Nothing to report upward — this is the `None` the snapshot maps to JSON
        // `null`. `max_utilization` reads the same either way (an absent window and
        // a `Some(0.0)` both leave the fold at `0.0`); the difference is on the
        // wire and in the reset-derived signals asserted above.
        assert_eq!(quota.max_utilization(now, false), 0.0);
    }

    /// The same unreadable bucket must still PRESERVE a prior reading — that half
    /// of the merge (the JS `applyUsageData` behaviour) is not what was broken.
    ///
    /// The probe here reports a NEW reset alongside the unreadable utilization,
    /// which is what makes this test able to fail: skipping the merge entirely on
    /// an unreadable utilization would also preserve the prior utilization (by not
    /// writing at all) and would be indistinguishable — except that the freshly
    /// reported reset would be dropped on the floor.
    #[test]
    fn unreadable_utilization_preserves_a_prior_reading() {
        let now = OffsetDateTime::now_utc();
        let prior_reset = now + Duration::hours(3);
        let reported_reset = now + Duration::hours(4);
        let mut quota = Quota {
            five_hour: Some(QuotaWindow {
                utilization: 0.77,
                reset: Some(prior_reset),
            }),
            ..Quota::default()
        };
        quota.apply_usage(&crate::probe::Usage {
            five_hour: Some(crate::probe::UsageBucket {
                utilization: None,
                reset_at_ms: Some((reported_reset.unix_timestamp_nanos() / 1_000_000) as i64),
            }),
            ..Default::default()
        });

        let window = quota.five_hour.expect("the prior window must survive");
        assert_eq!(window.utilization, 0.77, "prior utilization kept");
        assert_eq!(
            window.reset.map(|r| r.unix_timestamp()),
            Some(reported_reset.unix_timestamp()),
            "and the freshly reported reset is still adopted"
        );
        // With no reported reset the still-future prior reset is inherited instead.
        let mut quota = Quota {
            five_hour: Some(QuotaWindow {
                utilization: 0.77,
                reset: Some(prior_reset),
            }),
            ..Quota::default()
        };
        quota.apply_usage(&crate::probe::Usage {
            five_hour: Some(crate::probe::UsageBucket {
                utilization: None,
                reset_at_ms: None,
            }),
            ..Default::default()
        });
        assert_eq!(
            quota.five_hour.map(|w| (w.utilization, w.reset)),
            Some((0.77, Some(prior_reset))),
            "prior utilization and prior reset both intact"
        );
    }

    /// The guard above must not swing the error the other way: a GENUINE numeric
    /// zero is a reading, and must stay a present `0.0` window rather than
    /// collapsing into "unmeasured". This is the test that pins the two apart.
    #[test]
    fn a_genuine_zero_utilization_is_a_reading_not_an_absence() {
        let mut quota = Quota::default();
        quota.apply_usage(&crate::probe::Usage {
            five_hour: Some(crate::probe::UsageBucket {
                utilization: Some(0.0),
                reset_at_ms: None,
            }),
            ..Default::default()
        });

        let window = quota
            .five_hour
            .expect("a genuine 0% is a measurement — the window must be present");
        assert_eq!(window.utilization, 0.0);
        // A later unreadable probe then falls into the prior-preserving arm, so
        // the measured zero survives instead of blanking the bar.
        quota.apply_usage(&crate::probe::Usage {
            five_hour: Some(crate::probe::UsageBucket {
                utilization: None,
                reset_at_ms: None,
            }),
            ..Default::default()
        });
        assert_eq!(
            quota.five_hour.map(|w| w.utilization),
            Some(0.0),
            "a measured zero is not re-classified as unmeasured by later silence"
        );
    }

    /// Finding #3: a spent standard (API-key) limit must read fresh once its
    /// `standard_reset` has passed, or the account is pinned out permanently
    /// (no OAuth probe re-reads API-key limits).
    #[test]
    fn standard_limit_reads_fresh_after_reset() {
        let now = OffsetDateTime::now_utc();
        let spent = Quota {
            tokens_limit: Some(1000),
            tokens_remaining: Some(0),
            standard_reset: Some(now - Duration::hours(1)),
            ..Quota::default()
        };
        assert!(
            !spent.is_near(0.90, now),
            "a standard window past its reset must not keep gating"
        );

        // Before the reset the same spent window still gates.
        let unexpired = Quota {
            standard_reset: Some(now + Duration::hours(1)),
            ..spent
        };
        assert!(unexpired.is_near(0.90, now));
    }

    /// Per-model routing: `model_weekly_exhausted` gates on the model-scoped
    /// `7d_oi` (Fable) bucket. True when the bucket is at/over threshold with a
    /// future reset; false when the bucket was never learned.
    #[test]
    fn model_weekly_exhausted_true_when_oi_over_threshold() {
        let now = OffsetDateTime::now_utc();
        let quota = Quota {
            seven_day_oi: Some(QuotaWindow {
                utilization: 0.95,
                reset: Some(now + Duration::hours(2)),
            }),
            ..Quota::default()
        };
        assert!(quota.model_weekly_exhausted(0.90, now));
    }

    #[test]
    fn model_weekly_exhausted_false_when_oi_absent() {
        let now = OffsetDateTime::now_utc();
        assert!(!Quota::default().model_weekly_exhausted(0.90, now));
    }

    /// Bug #2 applies here too: a `7d_oi` bucket past its reset reads as a fresh
    /// `0.0`, so an exhausted-then-reset Fable bucket must NOT keep gating.
    #[test]
    fn model_weekly_exhausted_reads_fresh_after_reset() {
        let now = OffsetDateTime::now_utc();
        let quota = Quota {
            seven_day_oi: Some(QuotaWindow {
                utilization: 0.95,
                reset: Some(now - Duration::hours(1)), // reset already passed
            }),
            ..Quota::default()
        };
        assert!(!quota.model_weekly_exhausted(0.90, now));
    }

    /// Fable weekly gate (three-tier): `model_weekly_rejected` is the AUTHORITY
    /// check, distinct from the PREDICTION `model_weekly_exhausted` — a bucket
    /// sitting at 0.96 (over a 0.95 threshold, the observed live-fleet shape)
    /// is exhausted-by-prediction but NOT upstream-rejected.
    #[test]
    fn model_weekly_rejected_false_when_only_over_threshold() {
        let now = OffsetDateTime::now_utc();
        let quota = Quota {
            seven_day_oi: Some(QuotaWindow {
                utilization: 0.96,
                reset: Some(now + Duration::hours(2)),
            }),
            ..Quota::default()
        };
        assert!(
            quota.model_weekly_exhausted(0.95, now),
            "0.96 is over the 0.95 threshold — predicted exhausted"
        );
        assert!(
            !quota.model_weekly_rejected(now),
            "0.96 is not 1.0 — upstream has not actually rejected"
        );
    }

    #[test]
    fn model_weekly_rejected_true_only_at_or_over_1_0() {
        let now = OffsetDateTime::now_utc();
        let quota = Quota {
            seven_day_oi: Some(QuotaWindow {
                utilization: 1.0,
                reset: Some(now + Duration::hours(2)),
            }),
            ..Quota::default()
        };
        assert!(quota.model_weekly_rejected(now));
    }

    #[test]
    fn model_weekly_rejected_false_when_oi_absent() {
        let now = OffsetDateTime::now_utc();
        assert!(!Quota::default().model_weekly_rejected(now));
    }

    /// A `7d_oi` rejection past its reset must read fresh (`0.0`), the same
    /// bug-#2 shape `model_weekly_exhausted_reads_fresh_after_reset` already
    /// guards for the prediction side.
    #[test]
    fn model_weekly_rejected_reads_fresh_after_reset() {
        let now = OffsetDateTime::now_utc();
        let quota = Quota {
            seven_day_oi: Some(QuotaWindow {
                utilization: 1.0,
                reset: Some(now - Duration::hours(1)),
            }),
            ..Quota::default()
        };
        assert!(!quota.model_weekly_rejected(now));
    }

    /// Regression guard: `is_near` must keep IGNORING the `7d_oi` bucket, so a
    /// full Fable weekly never gates the shared (non-Fable) rotation. Only the
    /// new `model_weekly_exhausted` — consulted for Fable requests alone — sees it.
    #[test]
    fn is_near_still_ignores_seven_day_oi() {
        let now = OffsetDateTime::now_utc();
        let quota = Quota {
            seven_day_oi: Some(QuotaWindow {
                utilization: 0.95,
                reset: Some(now + Duration::hours(2)),
            }),
            ..Quota::default()
        };
        assert!(!quota.is_near(0.90, now));
    }

    /// The exact header block from issue #178, applied the way a 429 applies it:
    /// the account must come out blocked for Fable and eligible for everything
    /// else. Both halves are asserted, because the bug was that the shared
    /// windows went down with the model-scoped one.
    #[test]
    fn the_issue_178_header_block_blocks_only_fable() {
        let now = OffsetDateTime::now_utc();
        let headers = TestHeaders::new(&[
            ("anthropic-ratelimit-unified-status", "rejected"),
            ("anthropic-ratelimit-unified-5h-status", "allowed"),
            ("anthropic-ratelimit-unified-7d-status", "allowed_warning"),
            ("anthropic-ratelimit-unified-7d_oi-status", "rejected"),
        ]);
        let mut quota = Quota::default();
        let _ = quota.update_from_headers(&headers);
        quota.reject_model_weekly(&headers);

        assert!(
            quota.model_weekly_exhausted(0.90, now),
            "the Fable weekly scope must read exhausted"
        );
        assert!(
            !quota.is_near(0.90, now),
            "the shared windows must stay allowed, so Opus and Sonnet still route here"
        );
        assert_ne!(
            quota.status.as_deref(),
            Some("rejected"),
            "the aggregate must not be stored account-wide when a sub-scope explains it \
             — `Manager::account_terminal_gate` reads it as a terminal gate"
        );
    }

    /// Step 5/6 of issue #178's repro: a later response whose `7d_oi` scope reads
    /// allowed clears the rejection. The window itself must survive — dropping it
    /// would lose the `reset` that the exhausted-fleet response reports as "frees
    /// at", which is the regression F-04 of the branch review named.
    #[test]
    fn allowed_seven_day_oi_status_clears_the_rejection_without_nulling_the_window() {
        let now = OffsetDateTime::now_utc();
        let reset_secs = (now + Duration::days(3)).unix_timestamp();
        let mut quota = Quota::default();
        quota.reject_model_weekly(&TestHeaders::new(&[(
            "anthropic-ratelimit-unified-7d_oi-reset",
            &reset_secs.to_string(),
        )]));
        assert!(quota.model_weekly_exhausted(0.90, now));

        let allowed = TestHeaders::new(&[
            ("anthropic-ratelimit-unified-status", "allowed"),
            ("anthropic-ratelimit-unified-5h-status", "allowed"),
            ("anthropic-ratelimit-unified-7d-status", "allowed"),
            ("anthropic-ratelimit-unified-7d_oi-status", "allowed"),
        ]);
        let _ = quota.update_from_headers(&allowed);

        assert!(
            !quota.model_weekly_exhausted(0.90, now),
            "an allowed verdict on this scope must free Fable again"
        );
        let window = quota
            .seven_day_oi
            .expect("the learned window must survive the recovery, reset and all");
        assert_eq!(window.utilization, 0.0);
        assert_eq!(
            window.reset.map(OffsetDateTime::unix_timestamp),
            Some(reset_secs),
            "the known reset must be inherited, not re-synthesized"
        );
    }

    // --- Codex: the `x-codex-*` quota headers --------------------------------

    /// A Codex response's six window headers as `openai/codex` names them
    /// (`codex-api/src/rate_limits.rs:68-79`): the primary window 300 minutes and
    /// the secondary 10 080, tcr's five-hour and weekly lengths. Used values are
    /// percents (0–100); resets are epoch seconds.
    fn codex_headers(
        primary_used: &str,
        secondary_used: &str,
        reset_primary: i64,
        reset_secondary: i64,
    ) -> TestHeaders {
        TestHeaders::new(&[
            ("x-codex-primary-used-percent", primary_used),
            ("x-codex-primary-window-minutes", "300"),
            ("x-codex-primary-reset-at", &reset_primary.to_string()),
            ("x-codex-secondary-used-percent", secondary_used),
            ("x-codex-secondary-window-minutes", "10080"),
            ("x-codex-secondary-reset-at", &reset_secondary.to_string()),
        ])
    }

    /// The two-window shape the source describes: each percent becomes the
    /// fraction every threshold comparison already speaks (25 → 0.25), each reset
    /// is taken as reported, the model-scoped bucket stays empty, and a 200 reads
    /// as `allowed`.
    #[test]
    fn codex_headers_fill_the_five_hour_and_weekly_windows_as_fractions() {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let mut q = Quota::default();
        let read =
            q.update_from_codex_headers(&codex_headers("25", "3.5", now + 3600, now + 86400), 200);
        assert!(read, "a primary window was read");
        let fh = q.five_hour.expect("five_hour");
        assert!(
            (fh.utilization - 0.25).abs() < 1e-9,
            "25 percent is 0.25, got {}",
            fh.utilization
        );
        assert_eq!(fh.reset.unwrap().unix_timestamp(), now + 3600);
        let sd = q.seven_day.expect("seven_day");
        assert!(
            (sd.utilization - 0.035).abs() < 1e-9,
            "3.5 percent is 0.035, got {}",
            sd.utilization
        );
        assert_eq!(sd.reset.unwrap().unix_timestamp(), now + 86400);
        assert!(
            q.seven_day_oi.is_none(),
            "Codex has no model-scoped weekly bucket"
        );
        assert_eq!(q.status.as_deref(), Some("allowed"));
    }

    /// The mapping is by LENGTH: a plan that lists its weekly window first still
    /// lands each number in the right bucket, and the five-hour read is reported
    /// from whichever slot carried it.
    #[test]
    fn codex_windows_are_mapped_by_length_not_position() {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let swapped = TestHeaders::new(&[
            ("x-codex-primary-used-percent", "40"),
            ("x-codex-primary-window-minutes", "10080"),
            ("x-codex-primary-reset-at", &(now + 86400).to_string()),
            ("x-codex-secondary-used-percent", "10"),
            ("x-codex-secondary-window-minutes", "300"),
            ("x-codex-secondary-reset-at", &(now + 600).to_string()),
        ]);
        let mut q = Quota::default();
        assert!(
            q.update_from_codex_headers(&swapped, 200),
            "the five-hour window was read, from the secondary slot"
        );
        assert!((q.five_hour.unwrap().utilization - 0.10).abs() < 1e-9);
        assert!((q.seven_day.unwrap().utilization - 0.40).abs() < 1e-9);
    }

    /// With no `window-minutes` the slot decides: primary is the five-hour window,
    /// secondary the weekly. A window reported without its reset gets the bounded
    /// one [`resolve_reset`] synthesizes, never none (bugs #4/#5), and of ITS
    /// bucket's length: five hours for the primary slot, seven days for the
    /// secondary. Swapping the two lengths in the parser fails the reset checks.
    #[test]
    fn codex_windows_without_minutes_fall_back_to_position() {
        let only_pct = TestHeaders::new(&[
            ("x-codex-primary-used-percent", "50"),
            ("x-codex-secondary-used-percent", "5"),
        ]);
        let mut q = Quota::default();
        let before = OffsetDateTime::now_utc();
        assert!(
            q.update_from_codex_headers(&only_pct, 200),
            "by position the primary slot is the five-hour window"
        );
        let after = OffsetDateTime::now_utc();
        assert!((q.five_hour.unwrap().utilization - 0.5).abs() < 1e-9);
        assert!((q.seven_day.unwrap().utilization - 0.05).abs() < 1e-9);
        let five_hour_reset = q.five_hour.unwrap().reset.expect("a synthesized reset");
        assert!(
            five_hour_reset >= before + FIVE_HOUR && five_hour_reset <= after + FIVE_HOUR,
            "the five-hour window resets one five-hour length out, got {five_hour_reset}"
        );
        let weekly_reset = q.seven_day.unwrap().reset.expect("a synthesized reset");
        assert!(
            weekly_reset >= before + SEVEN_DAY && weekly_reset <= after + SEVEN_DAY,
            "the weekly window resets one seven-day length out, got {weekly_reset}"
        );
    }

    /// The shape the backend may send for an absent window, which no inference
    /// has measured yet: a `0` in the secondary slot beside the real weekly window
    /// in the primary slot. Placeholder or not, it carries no data, so it neither
    /// overwrites the weekly reading (97% must not become 0%) nor fabricates a
    /// five-hour window (a `window-minutes: 0` is no length, never a short one).
    /// Run once with `window-minutes: 0` and once with no length at all.
    ///
    /// Then the same placeholder in the PRIMARY slot, beside the weekly window
    /// read by its length from the secondary slot. That is the case only the
    /// no-data skip decides: with no length the primary slot falls to the
    /// five-hour bucket by position, where no merge rescues it, and nothing else
    /// stops it.
    ///
    /// Watched red with the length rule, the no-data skip and the same-bucket
    /// merge all dropped: the first run fabricates a 0% five-hour window and
    /// answers `true`, and the second, run alone, overwrites the weekly 0.97 with
    /// 0.0. Watched red with the no-data skip ALONE dropped: the primary-slot
    /// case fabricates a 0% five-hour window and answers `true`, while both
    /// secondary-slot runs still pass.
    #[test]
    fn a_zero_codex_placeholder_slot_neither_overwrites_nor_fabricates_a_window() {
        let secs = OffsetDateTime::now_utc().unix_timestamp();
        for placeholder_minutes in [Some("0"), None] {
            let mut h = TestHeaders::new(&[
                ("x-codex-primary-used-percent", "97"),
                ("x-codex-primary-window-minutes", "10080"),
                ("x-codex-primary-reset-at", &(secs + 86_400).to_string()),
                ("x-codex-secondary-used-percent", "0"),
            ]);
            if let Some(minutes) = placeholder_minutes {
                h.push("x-codex-secondary-window-minutes", minutes);
            }
            let mut q = Quota::default();
            assert!(
                !q.update_from_codex_headers(&h, 200),
                "no five-hour window was read (window-minutes {placeholder_minutes:?})"
            );
            assert!(
                q.five_hour.is_none(),
                "no five-hour window is fabricated (window-minutes {placeholder_minutes:?})"
            );
            let weekly = q.seven_day.expect("the weekly window");
            assert!(
                (weekly.utilization - 0.97).abs() < 1e-9,
                "the weekly reading stays 0.97 (window-minutes {placeholder_minutes:?}), got {}",
                weekly.utilization
            );
            assert_eq!(weekly.reset.unwrap().unix_timestamp(), secs + 86_400);
        }

        let primary_placeholder = TestHeaders::new(&[
            ("x-codex-primary-used-percent", "0"),
            ("x-codex-secondary-used-percent", "97"),
            ("x-codex-secondary-window-minutes", "10080"),
            ("x-codex-secondary-reset-at", &(secs + 86_400).to_string()),
        ]);
        let mut q = Quota::default();
        assert!(
            !q.update_from_codex_headers(&primary_placeholder, 200),
            "no five-hour window was read from a primary slot that carries no data"
        );
        assert!(
            q.five_hour.is_none(),
            "a primary slot with no data fabricates no five-hour window by position"
        );
        let weekly = q
            .seven_day
            .expect("the weekly window, read from the secondary slot");
        assert!(
            (weekly.utilization - 0.97).abs() < 1e-9,
            "the weekly reading stays 0.97, got {}",
            weekly.utilization
        );
        assert_eq!(weekly.reset.unwrap().unix_timestamp(), secs + 86_400);
    }

    /// Two slots that land in ONE bucket keep the fuller reading and the later
    /// reset, in either order: two weekly-length windows (10 080 and 43 200
    /// minutes), and a weekly window beside a positional one. The last slot
    /// written used to win, so the order of the headers decided which reading
    /// survived.
    ///
    /// Watched red: let each slot overwrite its bucket again and the fuller
    /// reading is lost whenever it is listed first.
    #[test]
    fn two_codex_slots_in_one_bucket_keep_the_fuller_reading_and_the_later_reset() {
        let secs = OffsetDateTime::now_utc().unix_timestamp();
        let (early, late) = (secs + 86_400, secs + 200_000);
        for (first, second) in [
            (("90", "10080", late), ("40", "43200", early)),
            (("40", "10080", early), ("90", "43200", late)),
        ] {
            let h = TestHeaders::new(&[
                ("x-codex-primary-used-percent", first.0),
                ("x-codex-primary-window-minutes", first.1),
                ("x-codex-primary-reset-at", &first.2.to_string()),
                ("x-codex-secondary-used-percent", second.0),
                ("x-codex-secondary-window-minutes", second.1),
                ("x-codex-secondary-reset-at", &second.2.to_string()),
            ]);
            let mut q = Quota::default();
            assert!(!q.update_from_codex_headers(&h, 200));
            let weekly = q.seven_day.expect("the weekly bucket");
            assert!(
                (weekly.utilization - 0.90).abs() < 1e-9,
                "the fuller reading survives, got {}",
                weekly.utilization
            );
            assert_eq!(weekly.reset.unwrap().unix_timestamp(), late);
            assert!(q.five_hour.is_none());
        }

        // A length-mapped weekly window, then a positional one (no length) in the
        // secondary slot: the positional reading may not overwrite the fuller one.
        let h = TestHeaders::new(&[
            ("x-codex-primary-used-percent", "97"),
            ("x-codex-primary-window-minutes", "10080"),
            ("x-codex-primary-reset-at", &late.to_string()),
            ("x-codex-secondary-used-percent", "5"),
            ("x-codex-secondary-reset-at", &early.to_string()),
        ]);
        let mut q = Quota::default();
        assert!(!q.update_from_codex_headers(&h, 200));
        let weekly = q.seven_day.expect("the weekly bucket");
        assert!(
            (weekly.utilization - 0.97).abs() < 1e-9,
            "got {}",
            weekly.utilization
        );
        assert_eq!(weekly.reset.unwrap().unix_timestamp(), late);
    }

    /// A Codex 429 WITHOUT `x-codex-rate-limit-reached-type` is not a quota
    /// refusal, so it leaves `status` exactly as it was: still `allowed` after a
    /// 200, still unset on a fresh account. Marking it `rejected` would put a
    /// transient 429 behind a terminal gate nothing clears.
    ///
    /// Watched red: drop the reached-type check from the `rejected` arm.
    #[test]
    fn a_codex_429_without_a_reached_type_leaves_status_alone() {
        let secs = OffsetDateTime::now_utc().unix_timestamp();
        let h = codex_headers("30", "100", secs + 600, secs + 50_000);
        let mut q = Quota::default();
        let _ = q.update_from_codex_headers(&h, 200);
        assert_eq!(q.status.as_deref(), Some("allowed"), "precondition");
        let _ = q.update_from_codex_headers(&h, 429);
        assert_eq!(
            q.status.as_deref(),
            Some("allowed"),
            "a 429 that is not a quota refusal leaves the status as it was"
        );
        let mut fresh = Quota::default();
        let _ = fresh.update_from_codex_headers(&h, 429);
        assert_eq!(fresh.status, None, "and sets none on a fresh account");
    }

    /// Both windows spent: the account serves again only when BOTH have rolled,
    /// so the hold runs to the later reset, whichever slot lists it.
    ///
    /// Watched red: let the first full window win again and the primary slot's
    /// earlier reset holds the account for ten minutes instead of the weekly's.
    #[test]
    fn codex_hold_takes_the_later_reset_when_both_windows_are_full() {
        let now = OffsetDateTime::now_utc();
        let secs = now.unix_timestamp();
        for (primary_reset, secondary_reset) in
            [(secs + 600, secs + 50_000), (secs + 50_000, secs + 600)]
        {
            let h = codex_headers("100", "100", primary_reset, secondary_reset);
            let hold = codex_hold_seconds(&h, now).expect("a hold");
            assert!(
                (49_995..=50_000).contains(&hold),
                "both windows are spent, so the hold runs to the later reset, got {hold}"
            );
        }
    }

    /// A refusal: a 429 carrying `x-codex-rate-limit-reached-type` is the one shape
    /// that marks the quota `rejected`, and the hold runs to the reset of the
    /// window that is full.
    #[test]
    fn a_codex_429_with_a_reached_type_marks_the_quota_rejected_and_names_the_hold() {
        let now = OffsetDateTime::now_utc();
        let secs = now.unix_timestamp();
        let mut h = codex_headers("100", "12", secs + 1800, secs + 86400);
        h.push("x-codex-rate-limit-reached-type", "primary");
        let mut q = Quota::default();
        assert!(
            q.update_from_codex_headers(&h, 429),
            "a 429's windows are folded exactly like a 200's"
        );
        assert_eq!(q.status.as_deref(), Some("rejected"));
        let hold = codex_hold_seconds(&h, now).expect("a hold");
        assert!(
            (1795..=1800).contains(&hold),
            "hold runs to the exhausted window's reset, got {hold}"
        );
    }

    /// The hold follows the window that is FULL, not the slot listed first: a
    /// five-hour window at 30% does not free an account whose weekly is at 100%.
    #[test]
    fn codex_hold_prefers_the_window_that_is_actually_full() {
        let now = OffsetDateTime::now_utc();
        let secs = now.unix_timestamp();
        let h = codex_headers("30", "100", secs + 600, secs + 50_000);
        let hold = codex_hold_seconds(&h, now).unwrap();
        assert!(
            (49_995..=50_000).contains(&hold),
            "hold runs to the full weekly window's reset, got {hold}"
        );
    }

    /// Each parser reads only its own provider's family, so a response folded
    /// through the wrong one fabricates nothing: an Anthropic header is silence to
    /// the Codex parser (no window, and so no `allowed` either), and a Codex block
    /// is silence to the Anthropic one.
    #[test]
    fn anthropic_headers_do_not_feed_the_codex_parser_and_vice_versa() {
        let anthropic = TestHeaders::new(&[("anthropic-ratelimit-unified-5h-utilization", "0.5")]);
        let mut q = Quota::default();
        assert!(!q.update_from_codex_headers(&anthropic, 200));
        assert!(q.five_hour.is_none());
        assert!(
            q.status.is_none(),
            "a 200 that carried no Codex window is no evidence about the account"
        );
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let mut q2 = Quota::default();
        assert!(!q2.update_from_headers(&codex_headers("25", "3", now + 1, now + 2)));
        assert!(q2.five_hour.is_none());
    }

    /// The shape the real backend sends, measured 2026-10-12 on a Pro plan with the
    /// login exhausted and again at 0%: ONLY the weekly window, in the PRIMARY slot,
    /// and no secondary at all. Length mapping lands it in `seven_day` (by position
    /// it would have filled `five_hour` with a weekly number and a weekly reset); no
    /// five-hour window was read, so the method answers `false`; and the status still
    /// follows the response, because a 200 is evidence whichever slot carried it.
    #[test]
    fn codex_weekly_only_in_the_primary_slot_fills_seven_day_and_leaves_five_hour_empty() {
        let now = OffsetDateTime::now_utc();
        let secs = now.unix_timestamp();
        let weekly_only = TestHeaders::new(&[
            ("x-codex-primary-used-percent", "100"),
            ("x-codex-primary-window-minutes", "10080"),
            ("x-codex-primary-reset-at", &(secs + 86400).to_string()),
        ]);
        let mut q = Quota::default();
        assert!(
            !q.update_from_codex_headers(&weekly_only, 200),
            "no five-hour window was read"
        );
        assert!(
            q.five_hour.is_none(),
            "a weekly window never lands in five_hour"
        );
        let sd = q.seven_day.expect("the weekly window lands in seven_day");
        assert!(
            (sd.utilization - 1.0).abs() < 1e-9,
            "100 percent is 1.0, got {}",
            sd.utilization
        );
        assert_eq!(sd.reset.unwrap().unix_timestamp(), secs + 86400);
        assert_eq!(
            q.status.as_deref(),
            Some("allowed"),
            "a 200 reads as allowed whichever slot carried the window"
        );
        let hold = codex_hold_seconds(&weekly_only, now).expect("a hold");
        assert!(
            (86_395..=86_400).contains(&hold),
            "the hold names the weekly window's reset, got {hold}"
        );
    }

    /// Codex's numbers follow this module's rules for Anthropic's (Finding #11): a
    /// non-finite `used-percent` is ABSENT. It creates no window, and it cannot
    /// outrank a real 100% when the hold picks its window — NaN compares false, so a
    /// NaN in the first slot would otherwise keep that slot and hold the account to
    /// the wrong reset. And a reset no clock can reach is no hold, not an overflow.
    #[test]
    fn codex_non_finite_or_unreachable_numbers_are_absent_not_fatal() {
        let now = OffsetDateTime::now_utc();
        let secs = now.unix_timestamp();
        let nan_first = codex_headers("nan", "100", secs + 600, secs + 50_000);
        let mut q = Quota::default();
        assert!(
            !q.update_from_codex_headers(&nan_first, 200),
            "a NaN five-hour window is not a read"
        );
        assert!(q.five_hour.is_none(), "and it creates no window");
        let hold = codex_hold_seconds(&nan_first, now).expect("a hold");
        assert!(
            (49_995..=50_000).contains(&hold),
            "the window that is really full still names the hold, got {hold}"
        );

        let unreachable = TestHeaders::new(&[
            ("x-codex-primary-used-percent", "100"),
            ("x-codex-primary-reset-at", &i64::MIN.to_string()),
        ]);
        assert_eq!(
            codex_hold_seconds(&unreachable, now),
            Some(0),
            "a reset before any clock is already past: no hold, and no overflow"
        );
    }
}
