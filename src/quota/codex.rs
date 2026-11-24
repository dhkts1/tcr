//! A Codex account's quota: the `x-codex-*` response headers, the refusal a 429 carries,
//! and the facts beside the windows that the status wire reports.

use time::{Duration, OffsetDateTime};

use super::{FIVE_HOUR, HeaderView, Quota, QuotaWindow, SEVEN_DAY, get_parsed, resolve_reset};

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
