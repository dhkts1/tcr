//! The Codex quota probe: one `GET /backend-api/wham/usage` per account, no inference.
//!
//! The Codex TUI reads its own limits from this endpoint
//! (`openai/codex` `backend-client/src/client/rate_limit_resets.rs:69-80,124-128`), with the
//! same bearer and `chatgpt-account-id` header as inference. The payload is
//! `{ plan_type, rate_limit: { allowed, limit_reached, primary_window, secondary_window },
//! credits }` and each window is `{ used_percent, limit_window_seconds, reset_after_seconds,
//! reset_at }` (`codex-backend-openapi-models/src/models/rate_limit_status_details.rs`,
//! `rate_limit_window_snapshot.rs`). Windows map onto tcr's buckets by LENGTH, as the header
//! parser in `quota.rs` does, so the two readings of the same account agree.
//!
//! Do not expect a five-hour window from it. Measured 2026-10-12 on two Pro logins
//! (`scripts/codex-contract-probe.sh`), one exhausted (`used_percent` 100) and one idle (0),
//! `rate_limit` carried ONLY the weekly window (`limit_window_seconds` 604800), in the PRIMARY
//! slot, and `secondary_window` was `null` in both. Beside `rate_limit`, the exhausted login's
//! payload carried thirteen other top-level keys (`model_usage`, `credits`, `spend_control`,
//! `rate_limit_upsell`, …). So on that plan a probe fills `seven_day` and leaves `five_hour`
//! absent, never a fabricated 0%: a `null` window is skipped and an unknown key is ignored.
//!
//! Beside the windows, the probe now reads four of those keys, which it used to ignore, into
//! [`CodexProbeFacts`] for the status wire: `plan_type` (which keeps the account's ChatGPT plan
//! current), `credits`, `spend_control` and `rate_limit_reached_type`, in the shapes the same
//! measurement printed (`credits` `{ has_credits, unlimited, balance, … }` with a string
//! balance, `spend_control` `{ reached, … }`, `rate_limit_reached_type` `{ type, details }`). It
//! also records which windows the payload carried and how long each is, so a plan that reports
//! only its weekly window, or none, is told apart from one whose windows are not known yet. The
//! other keys are still ignored.
//!
//! It reads two flags of `rate_limit` too, `allowed` and `limit_reached`: the backend's own
//! verdict on the account. A probe that reads the account allowed, not at its limit, with its
//! spend control not reached and no refusal kind, ends a standing Codex refusal
//! ([`crate::quota::Quota::apply_usage`]), so a probe ends a refusal no window explains, a
//! spend cap or credits, once ChatGPT ends it.

use serde_json::Value;

use crate::probe::{ProbeError, Usage, UsageBucket};

/// `limit_window_seconds` at or under this is the five-hour bucket; above it, the weekly one.
pub const CODEX_FIVE_HOUR_CUTOFF_SECONDS: i64 = 36_000;

/// What one Codex usage probe said about the account beyond its windows' utilization, which
/// [`crate::quota::Quota::apply_usage`] keeps for the status wire.
///
/// A Codex probe always carries one ([`usage_from_payload`] sets it on every return), and an
/// Anthropic probe never does, so `Usage::codex` being `Some` is what marks a Codex reading.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CodexProbeFacts {
    /// Which windows the plan reported, and how long each is. No window at all is a fact
    /// about the plan: a successful probe whose payload carries none reports exactly that.
    pub windows: crate::quota::CodexWindows,
    /// `plan_type`, verbatim (`pro`, `plus`, …): the account's ChatGPT plan as of this probe.
    /// `None` when the payload names no plan, and a string that is empty after trimming names
    /// none: taking it would blank the plan the account already has, and its label with it.
    pub plan_type: Option<String>,
    /// `rate_limit_reached_type`: its `type` string, or the value itself when it is a bare
    /// string.
    pub limit_reached_type: Option<String>,
    /// Whether `spend_control.reached` is `true`. Anything else is `false`.
    pub spend_control_reached: bool,
    /// The `credits` object, when it carries a boolean `has_credits`.
    pub credits: Option<tcr_status_wire::CodexCredits>,
    /// `rate_limit.allowed`: the backend's own verdict that the account may be served. Only a
    /// JSON boolean counts; anything else, and a payload with no `rate_limit`, is `None`.
    pub allowed: Option<bool>,
    /// `rate_limit.limit_reached`: whether the backend says the account is at its limit. Read
    /// as [`Self::allowed`] is.
    pub limit_reached: Option<bool>,
}

// The header parser's cutoff in seconds. If the two drift apart, the probe and the headers file
// the same window under different buckets and the account's two readings disagree.
const _: () =
    assert!(CODEX_FIVE_HOUR_CUTOFF_SECONDS == crate::quota::CODEX_FIVE_HOUR_CUTOFF_MINUTES * 60);

/// Read one account's Codex quota, spending no inference.
///
/// A non-2xx answer keeps its status, so a `401` reaches the manager as one and earns the
/// forced refresh and single retry `Manager::probe_account` gives any rejected bearer. The
/// request is bounded by [`crate::probe::PROBE_TIMEOUT`], as the Anthropic probe's is: the
/// scheduler awaits one probe at a time, so a stalled endpoint must not hold up every other
/// account's probe behind it.
///
/// A row with no account id sends no `chatgpt-account-id` at all, never a placeholder, exactly
/// as inference does (`proxy::build_codex_upstream_headers`), so the probe reads the account
/// the backend serves that bearer on. Like CodexBar's OAuth usage fetcher, the request identifies
/// its client with a `User-Agent`; tcr sends its own version rather than CodexBar's identity.
pub async fn fetch_usage(
    client: &reqwest::Client,
    base: &str,
    access_token: &str,
    account_id: Option<&str>,
) -> Result<Usage, ProbeError> {
    let url = format!(
        "{}{}",
        base.trim_end_matches('/'),
        crate::provider::CODEX_USAGE_PATH
    );
    let mut req = client
        .get(&url)
        .bearer_auth(access_token)
        .header(reqwest::header::ACCEPT, "application/json")
        .header(
            reqwest::header::USER_AGENT,
            concat!("tcr/", env!("CARGO_PKG_VERSION")),
        )
        .timeout(crate::probe::PROBE_TIMEOUT);
    if let Some(id) = account_id {
        req = req.header(crate::provider::CODEX_ACCOUNT_HEADER, id);
    }
    let response = req.send().await.map_err(|e| ProbeError {
        status: None,
        message: format!("codex usage probe transport: {e}"),
        retry_after_secs: None,
    })?;
    let status = response.status().as_u16();
    let retry_after_secs = crate::proxy::parse_retry_after(response.headers())
        .and_then(|seconds| u64::try_from(seconds).ok());
    let body = response.bytes().await.map_err(|e| ProbeError {
        status: Some(status),
        message: format!("codex usage probe body: {e}"),
        retry_after_secs,
    })?;
    if !(200..300).contains(&status) {
        return Err(ProbeError {
            status: Some(status),
            message: format!("codex usage probe HTTP {status}"),
            retry_after_secs,
        });
    }
    let payload: Value = serde_json::from_slice(&body).map_err(|e| ProbeError {
        status: Some(status),
        message: format!("codex usage probe was not JSON: {e}"),
        retry_after_secs: None,
    })?;
    Ok(usage_from_payload(&payload))
}

/// Map the payload onto tcr's buckets. `used_percent` is 0–100; `reset_at` is epoch seconds and
/// outranks `reset_after_seconds`, which is relative to a clock we did not observe.
///
/// A window is trusted only as far as it carries data, by the same three rules the header parser
/// applies (`Quota::update_from_codex_headers`), so the probe and the headers read one account
/// alike:
///
/// - a `limit_window_seconds` of zero or less is no length at all, and the slot decides;
/// - a window at 0% (or none) with no length and no reset carries no data and is skipped, the
///   rule Codex's own header reader applies. Read as a window it would fabricate a confident 0%
///   bucket;
/// - two windows that land in one bucket keep the higher utilization and the later reset, never
///   the last one read. A 0% placeholder must not turn a weekly reading of 97% into 0%, which
///   would also lift a live rejection that window still justifies.
///
/// Every return carries the probe's [`CodexProbeFacts`]: the plan, credits, spend control and
/// refusal kind are read first, then the windows the loop below kept, with each one's length.
/// That includes the return for a payload with no `rate_limit` at all: a successful probe that
/// carries no window says the plan reports none, which is a fact the status wire shows as `[]`,
/// not the unknown of a probe that never answered.
pub fn usage_from_payload(v: &Value) -> Usage {
    let mut facts = CodexProbeFacts {
        plan_type: v
            .get("plan_type")
            .and_then(Value::as_str)
            .filter(|plan| !plan.trim().is_empty())
            .map(str::to_string),
        limit_reached_type: reached_type_from_payload(v.get("rate_limit_reached_type")),
        spend_control_reached: v
            .get("spend_control")
            .and_then(|control| control.get("reached"))
            .and_then(Value::as_bool)
            == Some(true),
        credits: credits_from_payload(v.get("credits")),
        windows: crate::quota::CodexWindows::default(),
        allowed: None,
        limit_reached: None,
    };
    let mut usage = Usage::default();
    let Some(rate_limit) = v.get("rate_limit").filter(|r| !r.is_null()) else {
        usage.codex = Some(facts);
        return usage;
    };
    facts.allowed = rate_limit.get("allowed").and_then(Value::as_bool);
    facts.limit_reached = rate_limit.get("limit_reached").and_then(Value::as_bool);
    let now_s = crate::now_ms() / 1000;
    // The length each bucket keeps, by the header parser's rule (`quota::codex_kept_length`).
    let mut five_hour_minutes: Option<u32> = None;
    let mut seven_day_minutes: Option<u32> = None;
    for (key, positional_is_weekly) in [("primary_window", false), ("secondary_window", true)] {
        let Some(w) = rate_limit.get(key).filter(|w| !w.is_null()) else {
            continue;
        };
        let utilization = w
            .get("used_percent")
            .and_then(Value::as_f64)
            .map(|p| p / 100.0);
        let reset_at_ms = w
            .get("reset_at")
            .and_then(Value::as_i64)
            .or_else(|| {
                w.get("reset_after_seconds")
                    .and_then(Value::as_i64)
                    .map(|s| now_s + s)
            })
            .map(|s| s * 1000);
        // Zero or less is no length.
        let seconds = w
            .get("limit_window_seconds")
            .and_then(Value::as_i64)
            .filter(|&s| s > 0);
        // No data, by the header parser's rule (Codex's own), once a non-positive length counts
        // as none.
        if utilization.is_none_or(|u| u == 0.0) && seconds.is_none() && reset_at_ms.is_none() {
            continue;
        }
        let weekly = match seconds {
            Some(secs) => secs > CODEX_FIVE_HOUR_CUTOFF_SECONDS,
            None => positional_is_weekly,
        };
        let (bucket, length) = if weekly {
            (&mut usage.seven_day, &mut seven_day_minutes)
        } else {
            (&mut usage.five_hour, &mut five_hour_minutes)
        };
        // A length of zero or less is no length, in minutes as in seconds.
        let minutes = seconds
            .and_then(|secs| u32::try_from(secs / 60).ok())
            .filter(|&m| m > 0);
        // The bucket keeps the length of the window whose utilization it keeps. A missing
        // utilization ranks below any reading, as the merge below ranks it.
        let rank = |utilization: Option<f64>| utilization.unwrap_or(f64::NEG_INFINITY);
        *length = match *bucket {
            None => minutes,
            Some(other) => crate::quota::codex_kept_length(
                (rank(other.utilization), *length),
                (rank(utilization), minutes),
            ),
        };
        *bucket = Some(match bucket.take() {
            None => UsageBucket {
                utilization,
                reset_at_ms,
            },
            Some(other) => UsageBucket {
                utilization: match (other.utilization, utilization) {
                    (Some(a), Some(b)) => Some(a.max(b)),
                    (a, b) => a.or(b),
                },
                reset_at_ms: other.reset_at_ms.max(reset_at_ms),
            },
        });
    }
    facts.windows = crate::quota::CodexWindows {
        five_hour: usage.five_hour.map(|_| crate::quota::CodexWindow {
            minutes: five_hour_minutes,
        }),
        seven_day: usage.seven_day.map(|_| crate::quota::CodexWindow {
            minutes: seven_day_minutes,
        }),
    };
    usage.codex = Some(facts);
    usage
}

/// The refusal's kind from the payload's `rate_limit_reached_type`: its `type` string, or the
/// value itself when it is a bare string. `None` for `null`, a missing key, or any other shape.
fn reached_type_from_payload(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(kind) => Some(kind.clone()),
        Value::Object(object) => object
            .get("type")
            .and_then(Value::as_str)
            .map(str::to_string),
        _ => None,
    }
}

/// The payload's `credits` object as the status wire carries it. It counts only with a boolean
/// `has_credits`; `unlimited` is `false` unless it is `true`; `balance` is the backend's string,
/// verbatim, and `None` for anything else: `null`, missing, or another JSON type.
///
/// Only a string, because that is what the backend sends (the measured payload's `balance` is
/// a string, `scripts/codex-contract-probe.sh`), and because the wire promises the backend's
/// own text: a JSON number would come back re-rendered by serde_json, `12.50` as `"12.5"`,
/// which is not what ChatGPT wrote.
fn credits_from_payload(value: Option<&Value>) -> Option<tcr_status_wire::CodexCredits> {
    let credits = value?.as_object()?;
    Some(tcr_status_wire::CodexCredits {
        has_credits: credits.get("has_credits")?.as_bool()?,
        unlimited: credits.get("unlimited").and_then(Value::as_bool) == Some(true),
        balance: credits
            .get("balance")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAYLOAD: &str = r#"{"plan_type":"pro","rate_limit":{"allowed":true,"limit_reached":false,
        "primary_window":{"used_percent":25,"limit_window_seconds":18000,"reset_after_seconds":3600,"reset_at":1800003600},
        "secondary_window":{"used_percent":3,"limit_window_seconds":604800,"reset_after_seconds":86400,"reset_at":1800086400}},
        "credits":{"has_credits":false,"unlimited":false,"balance":null}}"#;

    #[test]
    fn payload_windows_land_in_five_hour_and_weekly_by_length() {
        let usage = usage_from_payload(&serde_json::from_str(PAYLOAD).unwrap());
        let fh = usage.five_hour.unwrap();
        assert!((fh.utilization.unwrap() - 0.25).abs() < 1e-9);
        assert_eq!(fh.reset_at_ms, Some(1_800_003_600_000));
        let sd = usage.seven_day.unwrap();
        assert!((sd.utilization.unwrap() - 0.03).abs() < 1e-9);
        assert_eq!(sd.reset_at_ms, Some(1_800_086_400_000));
        assert!(usage.seven_day_oi.is_none());
    }

    #[test]
    fn a_payload_without_rate_limit_is_an_empty_usage_not_a_zero() {
        let usage = usage_from_payload(&serde_json::json!({"plan_type":"free","rate_limit":null}));
        assert!(usage.five_hour.is_none() && usage.seven_day.is_none());
    }

    #[test]
    fn reset_after_seconds_is_the_fallback_when_reset_at_is_absent() {
        let v = serde_json::json!({"rate_limit":{"primary_window":{"used_percent":10,"limit_window_seconds":18000,"reset_after_seconds":120}}});
        let before = crate::now_ms();
        let usage = usage_from_payload(&v);
        let reset = usage.five_hour.unwrap().reset_at_ms.unwrap();
        assert!(reset >= before + 119_000 && reset <= before + 121_000 + 1000);
    }

    /// The shape the real backend sent, measured 2026-10-12 on two Pro logins, one exhausted
    /// (`used_percent` 100) and one idle (0), with `scripts/codex-contract-probe.sh`: ONLY the
    /// weekly window, in the PRIMARY slot, and `secondary_window` null. Beside `rate_limit` are
    /// thirteen other top-level keys, every nested key as the probe printed it for the exhausted
    /// login. Four of them (`plan_type`, `credits`, `spend_control`, `rate_limit_reached_type`)
    /// are read into the probe's facts, and none into a window.
    /// The probe printed a leaf's type, not its value
    /// (save `plan_type`, `allowed`, `limit_reached`, `used_percent` and
    /// `limit_window_seconds`), so each other leaf is a placeholder of that type and the three
    /// identity keys are fixtures. Of an array it printed only the length, so the two items
    /// of each are guesses. `rate_limit_upsell` holds a `reset_at` of its own, a later instant
    /// than the window's, which a parser that searched the payload for any `reset_at` could
    /// report instead.
    /// By length the primary slot is the weekly bucket; by position it would put a weekly
    /// number and a days-away reset into `five_hour`. In both states `five_hour` stays absent
    /// rather than a fabricated 0%, and no other window is filled.
    ///
    /// Watched red: map by position alone, ignoring `limit_window_seconds`, and the weekly
    /// reading lands in `five_hour`.
    #[test]
    fn the_measured_weekly_only_payload_fills_seven_day_from_the_primary_slot_and_nothing_else() {
        for (used, allowed, limit_reached) in [(100, false, true), (0, true, false)] {
            let payload = serde_json::json!({
                "user_id": "user-fixture",
                "account_id": "11111111-1111-4111-8111-111111111111",
                "email": "alice@example.com",
                "plan_type": "pro",
                "rate_limit": {
                    "allowed": allowed,
                    "limit_reached": limit_reached,
                    "primary_window": {
                        "used_percent": used,
                        "limit_window_seconds": 604_800,
                        "reset_after_seconds": 345_600,
                        "reset_at": 1_800_345_600,
                    },
                    "secondary_window": null,
                },
                "code_review_rate_limit": null,
                "additional_rate_limits": null,
                "model_usage": {
                    "gpt-6-astra": {
                        "available": false,
                        "available_at": "fixture",
                        "credits_would_enable": false,
                    },
                },
                "credits": {
                    "has_credits": false,
                    "unlimited": false,
                    "overage_limit_reached": false,
                    "balance": "0",
                    "approx_local_messages": [0, 0],
                    "approx_cloud_messages": [0, 0],
                },
                "spend_control": { "reached": false, "individual_limit": null },
                "rate_limit_reached_type": { "type": "fixture", "details": "fixture" },
                "rate_limit_upsell": {
                    "banner_type": "fixture",
                    "title": "fixture",
                    "description": "fixture",
                    "ctas": [{}, {}],
                    "reset_at": 1_800_900_000,
                    "referral": null,
                    "request_url": null,
                },
                "promo": null,
                "rate_limit_reset_credits": {
                    "available_count": 0,
                    "applicable_available_count": 0,
                },
            });
            let usage = usage_from_payload(&payload);
            let weekly = usage
                .seven_day
                .unwrap_or_else(|| panic!("the weekly window at {used}% must fill seven_day"));
            assert_eq!(
                weekly.utilization,
                Some(f64::from(used) / 100.0),
                "{used} percent"
            );
            assert_eq!(weekly.reset_at_ms, Some(1_800_345_600_000));
            assert!(
                usage.five_hour.is_none(),
                "no five-hour window was sent at {used}%, so none may be invented: {:?}",
                usage.five_hour
            );
            assert!(usage.seven_day_oi.is_none() && usage.extra_usage_usd.is_none());
        }
    }

    /// A placeholder window, `used_percent` 0 with no length and no reset, carries no data: beside
    /// the real weekly window it neither overwrites that reading nor fabricates a five-hour
    /// bucket. Run with the placeholder in each slot. Read by position, in the secondary slot it
    /// would land in the weekly bucket the real window filled, and in the PRIMARY slot in the
    /// five-hour bucket, where nothing but the no-data skip stops it.
    ///
    /// Watched red with the no-data skip alone dropped: the primary-slot placeholder fabricates a
    /// 0% five-hour bucket.
    #[test]
    fn a_zero_placeholder_window_neither_overwrites_nor_fabricates_a_bucket() {
        let weekly = serde_json::json!({
            "used_percent": 97,
            "limit_window_seconds": 604_800,
            "reset_at": 1_800_345_600,
        });
        let placeholder = serde_json::json!({ "used_percent": 0 });
        for (primary, secondary) in [(&weekly, &placeholder), (&placeholder, &weekly)] {
            let usage = usage_from_payload(&serde_json::json!({
                "rate_limit": { "primary_window": primary, "secondary_window": secondary },
            }));
            assert!(
                usage.five_hour.is_none(),
                "no five-hour bucket is fabricated (primary {primary}): {:?}",
                usage.five_hour
            );
            let read = usage.seven_day.expect("the weekly reading");
            assert!(
                (read.utilization.expect("a utilization") - 0.97).abs() < 1e-9,
                "the weekly reading stays 97% (primary {primary}): {read:?}"
            );
            assert_eq!(read.reset_at_ms, Some(1_800_345_600_000));
        }
    }

    /// A `limit_window_seconds` of 0 is no length, so the slot decides: a secondary window that
    /// reports 0 is the weekly bucket by position, never a five-hour window because 0 is under
    /// the cutoff. The primary window's 0 lands in the five-hour bucket either way: the control.
    ///
    /// Watched red: read a 0 as a length and the secondary window lands in `five_hour`.
    #[test]
    fn a_zero_window_length_falls_back_to_the_slots_position() {
        let usage = usage_from_payload(&serde_json::json!({
            "rate_limit": {
                "primary_window":
                    { "used_percent": 10, "limit_window_seconds": 0, "reset_at": 1_800_003_600 },
                "secondary_window":
                    { "used_percent": 40, "limit_window_seconds": 0, "reset_at": 1_800_345_600 },
            },
        }));
        let weekly = usage
            .seven_day
            .expect("the secondary window is the weekly bucket by position");
        assert!((weekly.utilization.expect("a utilization") - 0.40).abs() < 1e-9);
        assert_eq!(weekly.reset_at_ms, Some(1_800_345_600_000));
        let five_hour = usage
            .five_hour
            .expect("control: the primary window by position");
        assert!((five_hour.utilization.expect("a utilization") - 0.10).abs() < 1e-9);
        assert_eq!(five_hour.reset_at_ms, Some(1_800_003_600_000));
    }

    /// Two windows that land in ONE bucket keep the fuller reading and the later reset, in either
    /// order: two weekly-length windows, 7 and 30 days. Were the last window read to win, the
    /// order of the payload would decide which reading survived.
    ///
    /// Watched red: let each window overwrite its bucket and the fuller reading is lost whenever
    /// it is listed first.
    #[test]
    fn two_windows_in_one_bucket_keep_the_fuller_reading_and_the_later_reset() {
        let (early, late) = (1_800_086_400_i64, 1_800_200_000_i64);
        let window = |used: i64, seconds: i64, reset: i64| {
            serde_json::json!({
                "used_percent": used,
                "limit_window_seconds": seconds,
                "reset_at": reset,
            })
        };
        for (primary, secondary) in [
            (window(90, 604_800, late), window(40, 2_592_000, early)),
            (window(40, 604_800, early), window(90, 2_592_000, late)),
        ] {
            let usage = usage_from_payload(&serde_json::json!({
                "rate_limit": { "primary_window": primary, "secondary_window": secondary },
            }));
            let weekly = usage.seven_day.expect("the weekly bucket");
            assert!(
                (weekly.utilization.expect("a utilization") - 0.90).abs() < 1e-9,
                "the fuller reading survives: {weekly:?}"
            );
            assert_eq!(weekly.reset_at_ms, Some(late * 1000));
            assert!(usage.five_hour.is_none());
        }
    }

    /// A Codex probe's facts. Every Codex reading carries them, so their absence is a defect.
    fn facts(payload: &Value) -> CodexProbeFacts {
        usage_from_payload(payload)
            .codex
            .expect("a Codex probe always carries its facts")
    }

    /// The windows a probe's facts name, as the status wire names them.
    fn wire_names(facts: &CodexProbeFacts) -> Vec<String> {
        facts.windows.wire_names()
    }

    /// The measured Pro payload (copied from
    /// `the_measured_weekly_only_payload_fills_seven_day_from_the_primary_slot_and_nothing_else`,
    /// in both measured states) names the weekly window alone, seven days long: the plan has no
    /// five-hour window to draw.
    #[test]
    fn a_pro_probe_reports_the_weekly_window_only() {
        use crate::quota::{CodexWindow, CodexWindows};
        for (used, allowed, limit_reached) in [(100, false, true), (0, true, false)] {
            let payload = serde_json::json!({
                "user_id": "user-fixture",
                "account_id": "11111111-1111-4111-8111-111111111111",
                "email": "alice@example.com",
                "plan_type": "pro",
                "rate_limit": {
                    "allowed": allowed,
                    "limit_reached": limit_reached,
                    "primary_window": {
                        "used_percent": used,
                        "limit_window_seconds": 604_800,
                        "reset_after_seconds": 345_600,
                        "reset_at": 1_800_345_600,
                    },
                    "secondary_window": null,
                },
                "code_review_rate_limit": null,
                "additional_rate_limits": null,
                "model_usage": {
                    "gpt-6-astra": {
                        "available": false,
                        "available_at": "fixture",
                        "credits_would_enable": false,
                    },
                },
                "credits": {
                    "has_credits": false,
                    "unlimited": false,
                    "overage_limit_reached": false,
                    "balance": "0",
                    "approx_local_messages": [0, 0],
                    "approx_cloud_messages": [0, 0],
                },
                "spend_control": { "reached": false, "individual_limit": null },
                "rate_limit_reached_type": { "type": "fixture", "details": "fixture" },
                "rate_limit_upsell": {
                    "banner_type": "fixture",
                    "title": "fixture",
                    "description": "fixture",
                    "ctas": [{}, {}],
                    "reset_at": 1_800_900_000,
                    "referral": null,
                    "request_url": null,
                },
                "promo": null,
                "rate_limit_reset_credits": {
                    "available_count": 0,
                    "applicable_available_count": 0,
                },
            });
            let facts = facts(&payload);
            assert_eq!(
                facts.windows,
                CodexWindows {
                    five_hour: None,
                    seven_day: Some(CodexWindow {
                        minutes: Some(10_080)
                    }),
                },
                "at {used}%"
            );
            assert_eq!(wire_names(&facts), ["sevenDay"], "at {used}%");
        }
    }

    /// A Plus plan reports both windows, five hours and seven days, and its plan.
    #[test]
    fn a_plus_probe_reports_both_windows() {
        use crate::quota::CodexWindow;
        let facts = facts(&serde_json::json!({
            "plan_type": "plus",
            "rate_limit": {
                "allowed": true,
                "limit_reached": false,
                "primary_window": {
                    "used_percent": 18,
                    "limit_window_seconds": 18_000,
                    "reset_at": 1_800_003_600,
                },
                "secondary_window": {
                    "used_percent": 25,
                    "limit_window_seconds": 604_800,
                    "reset_at": 1_800_345_600,
                },
            },
        }));
        assert_eq!(
            facts.windows.five_hour,
            Some(CodexWindow { minutes: Some(300) })
        );
        assert_eq!(
            facts.windows.seven_day,
            Some(CodexWindow {
                minutes: Some(10_080)
            })
        );
        assert_eq!(wire_names(&facts), ["fiveHour", "sevenDay"]);
        assert_eq!(facts.plan_type.as_deref(), Some("plus"));
    }

    /// A successful probe that carries no window says the plan reports none: a `rate_limit`
    /// that is `null`, one whose windows are both `null`, and one whose windows are both
    /// placeholders. That is a fact (the wire's `[]`), not an unknown, and the plan still comes
    /// through on the early return.
    ///
    /// Watched red: return the usage without its facts on the `rate_limit: null` path and the
    /// first payload fails on the `expect`.
    #[test]
    fn a_probe_with_no_rate_limit_reports_no_window() {
        use crate::quota::CodexWindows;
        let no_rate_limit = serde_json::json!({"plan_type": "business", "rate_limit": null});
        let null_windows = serde_json::json!({
            "plan_type": "business",
            "rate_limit": { "primary_window": null, "secondary_window": null },
        });
        let placeholders = serde_json::json!({
            "plan_type": "business",
            "rate_limit": {
                "primary_window": { "used_percent": 0 },
                "secondary_window": { "used_percent": 0 },
            },
        });
        for payload in [&no_rate_limit, &null_windows, &placeholders] {
            let facts = facts(payload);
            assert_eq!(facts.windows, CodexWindows::default(), "{payload}");
            assert!(wire_names(&facts).is_empty(), "{payload}");
        }
        assert_eq!(facts(&no_rate_limit).plan_type.as_deref(), Some("business"));
    }

    /// The facts beside the windows: the plan, the credits (the balance verbatim), spend control
    /// and the refusal's kind, each by its rule, and each absent or `false` when the payload
    /// does not say.
    #[test]
    fn the_probe_reads_plan_credits_reached_type_and_spend_control() {
        use tcr_status_wire::CodexCredits;
        let payload = |edit: &dyn Fn(&mut serde_json::Map<String, Value>)| {
            let mut v = serde_json::json!({
                "plan_type": "pro",
                "rate_limit": {
                    "allowed": false,
                    "limit_reached": true,
                    "primary_window": {
                        "used_percent": 100,
                        "limit_window_seconds": 604_800,
                        "reset_at": 1_800_345_600,
                    },
                    "secondary_window": null,
                },
                "credits": {
                    "has_credits": true,
                    "unlimited": false,
                    "overage_limit_reached": false,
                    "balance": "12.50",
                    "approx_local_messages": [0, 0],
                },
                "spend_control": { "reached": true, "individual_limit": null },
                "rate_limit_reached_type": { "type": "rate_limit_reached", "details": "fixture" },
            });
            edit(v.as_object_mut().expect("an object"));
            facts(&v)
        };

        let read = payload(&|_| {});
        assert_eq!(read.plan_type.as_deref(), Some("pro"));
        assert_eq!(
            (read.allowed, read.limit_reached),
            (Some(false), Some(true)),
            "the measured exhausted shape"
        );
        let allowed = payload(&|v| {
            v["rate_limit"]["allowed"] = true.into();
            v["rate_limit"]["limit_reached"] = false.into();
        });
        assert_eq!(
            (allowed.allowed, allowed.limit_reached),
            (Some(true), Some(false)),
            "an allowed shape"
        );
        let no_rate_limit = payload(&|v| {
            v.insert("rate_limit".into(), Value::Null);
        });
        assert_eq!(
            (no_rate_limit.allowed, no_rate_limit.limit_reached),
            (None, None),
            "no rate_limit carries no verdict"
        );
        let not_booleans = payload(&|v| {
            v["rate_limit"]["allowed"] = "true".into();
            v["rate_limit"]["limit_reached"] = 0.into();
        });
        assert_eq!(
            (not_booleans.allowed, not_booleans.limit_reached),
            (None, None),
            "only a JSON boolean counts"
        );
        assert_eq!(
            read.credits,
            Some(CodexCredits {
                has_credits: true,
                unlimited: false,
                balance: Some("12.50".into()),
            })
        );
        assert!(read.spend_control_reached);
        assert_eq!(
            read.limit_reached_type.as_deref(),
            Some("rate_limit_reached")
        );

        let bare = payload(&|v| {
            v.insert("rate_limit_reached_type".into(), "workspace_credits".into());
        });
        assert_eq!(
            bare.limit_reached_type.as_deref(),
            Some("workspace_credits"),
            "a bare string is the kind itself"
        );
        let null_kind = payload(&|v| {
            v.insert("rate_limit_reached_type".into(), Value::Null);
        });
        assert_eq!(null_kind.limit_reached_type, None);
        let null_credits = payload(&|v| {
            v.insert("credits".into(), Value::Null);
        });
        assert_eq!(null_credits.credits, None);
        let unlimited = payload(&|v| {
            v.insert(
                "credits".into(),
                serde_json::json!({"has_credits": true, "unlimited": true, "balance": null}),
            );
        });
        assert_eq!(
            unlimited.credits,
            Some(CodexCredits {
                has_credits: true,
                unlimited: true,
                balance: None,
            })
        );
        let number_balance = payload(&|v| {
            v.insert(
                "credits".into(),
                serde_json::json!({"has_credits": true, "unlimited": false, "balance": 12.50}),
            );
        });
        assert_eq!(
            number_balance.credits,
            Some(CodexCredits {
                has_credits: true,
                unlimited: false,
                balance: None,
            }),
            "a balance is the backend's own string; a JSON number is not one"
        );
        let no_spend_control = payload(&|v| {
            v.remove("spend_control");
        });
        assert!(!no_spend_control.spend_control_reached);
    }

    /// The probe keeps each window's length by the header parser's rule: two windows in one
    /// bucket (seven and thirty days here) keep the length of the fuller, the longer on equal
    /// readings, and a window that carried no reading never wins over one that did, in either
    /// order. A length under a minute is no length.
    ///
    /// Watched red: let the last window read set the length, and the fuller seven-day window
    /// listed first reports thirty days.
    #[test]
    fn the_probe_keeps_each_windows_length_by_the_headers_rule() {
        let window = |used: Option<i64>, seconds: i64| {
            let mut w = serde_json::json!({
                "limit_window_seconds": seconds,
                "reset_at": 1_800_345_600,
            });
            if let Some(used) = used {
                w["used_percent"] = used.into();
            }
            w
        };
        let weekly_minutes = |primary: Value, secondary: Value| {
            facts(&serde_json::json!({
                "rate_limit": { "primary_window": primary, "secondary_window": secondary },
            }))
            .windows
            .seven_day
            .and_then(|w| w.minutes)
        };
        for ((primary_used, secondary_used), kept) in [
            ((Some(90), Some(40)), 10_080),
            ((Some(40), Some(90)), 43_200),
            ((Some(50), Some(50)), 43_200),
            ((None, Some(10)), 43_200),
            ((Some(10), None), 10_080),
        ] {
            assert_eq!(
                weekly_minutes(
                    window(primary_used, 604_800),
                    window(secondary_used, 2_592_000)
                ),
                Some(kept),
                "7 days at {primary_used:?}%, 30 days at {secondary_used:?}%"
            );
        }

        let under_a_minute = facts(&serde_json::json!({
            "rate_limit": { "primary_window": window(Some(10), 30), "secondary_window": null },
        }));
        assert_eq!(
            under_a_minute.windows.five_hour.map(|w| w.minutes),
            Some(None)
        );
    }

    /// Once a probe has answered, the probe's windows stand, whatever a response's headers say,
    /// in either order. A Pro response may carry a five-hour slot at 0% WITH a length and a
    /// reset, which passes the no-data rule, while the plan has no such window: the probe, which
    /// reports the plan's own windows, is the one to believe. The probe's spend control and
    /// credits are kept too.
    ///
    /// Watched red: let the headers' windows win in `CodexQuotaFacts::windows`, and the first
    /// order reads both windows.
    #[test]
    fn the_probe_wins_a_tie_with_the_headers() {
        use crate::quota::Quota;
        use tcr_status_wire::CodexCredits;
        let secs = crate::now_ms() / 1000;
        let mut headers = reqwest::header::HeaderMap::new();
        for (name, value) in [
            ("x-codex-primary-used-percent", "0".to_string()),
            ("x-codex-primary-window-minutes", "300".to_string()),
            ("x-codex-primary-reset-at", (secs + 18_000).to_string()),
            ("x-codex-secondary-used-percent", "40".to_string()),
            ("x-codex-secondary-window-minutes", "10080".to_string()),
            ("x-codex-secondary-reset-at", (secs + 345_600).to_string()),
        ] {
            headers.insert(name, value.parse().expect("a header value"));
        }
        let pro_payload = serde_json::json!({
            "plan_type": "pro",
            "rate_limit": {
                "allowed": true,
                "limit_reached": false,
                "primary_window": {
                    "used_percent": 40,
                    "limit_window_seconds": 604_800,
                    "reset_at": secs + 345_600,
                },
                "secondary_window": null,
            },
            "credits": { "has_credits": true, "unlimited": false, "balance": "12.50" },
            "spend_control": { "reached": true, "individual_limit": null },
        });
        let names = |q: &Quota| q.codex.windows().map(|w| w.wire_names());
        let both = Some(vec!["fiveHour".to_string(), "sevenDay".to_string()]);
        let weekly_only = Some(vec!["sevenDay".to_string()]);

        let mut control = Quota::default();
        let _ = control.update_from_codex_headers(&headers, 200);
        assert_eq!(
            names(&control),
            both,
            "control: the headers alone carry both windows"
        );

        let mut headers_first = Quota::default();
        let _ = headers_first.update_from_codex_headers(&headers, 200);
        headers_first.apply_usage(&usage_from_payload(&pro_payload));
        assert_eq!(
            names(&headers_first),
            weekly_only,
            "headers, then the probe"
        );
        assert_eq!(
            headers_first.codex.windows().and_then(|w| w.five_hour),
            None
        );
        assert!(headers_first.codex.spend_control_reached);
        assert_eq!(
            headers_first.codex.credits,
            Some(CodexCredits {
                has_credits: true,
                unlimited: false,
                balance: Some("12.50".into()),
            })
        );

        let mut probe_first = Quota::default();
        probe_first.apply_usage(&usage_from_payload(&pro_payload));
        let _ = probe_first.update_from_codex_headers(&headers, 200);
        assert_eq!(names(&probe_first), weekly_only, "the probe, then headers");
    }

    #[tokio::test]
    async fn fetch_sends_tcr_user_agent() {
        use axum::{routing::get, Router};

        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let captured = seen.clone();
        let app = Router::new().route(
            "/backend-api/wham/usage",
            get(move |headers: axum::http::HeaderMap| async move {
                *captured.lock().unwrap() = headers
                    .get(axum::http::header::USER_AGENT)
                    .map(|value| value.to_str().unwrap().to_string());
                PAYLOAD
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();

        fetch_usage(&client, &base, "test-access-token", None)
            .await
            .unwrap();

        assert_eq!(
            seen.lock().unwrap().as_deref(),
            Some(concat!("tcr/", env!("CARGO_PKG_VERSION"))),
            "the usage request identifies tcr to the upstream"
        );
        server.abort();
    }

    /// The probe sends the bearer, and the row's account id as `chatgpt-account-id`; a row with
    /// no account id sends no such header, as inference does. A 401 keeps its status, and a
    /// 429's `retry-after` reaches the manager through the shared parser.
    ///
    /// Watched red, one break at a time: send a placeholder id for a row without one and the
    /// `None` call records it; drop the `retry-after` read and the 429 carries no wait.
    #[tokio::test]
    async fn fetch_sends_bearer_and_account_header_and_maps_401() {
        use axum::{response::IntoResponse, routing::get, Router};
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<(String, String)>::new()));
        let seen2 = seen.clone();
        let app = Router::new().route(
            "/backend-api/wham/usage",
            get(move |headers: axum::http::HeaderMap| {
                let seen2 = seen2.clone();
                async move {
                    let mut s = seen2.lock().unwrap();
                    for k in ["authorization", "chatgpt-account-id"] {
                        if let Some(v) = headers.get(k) {
                            s.push((k.to_string(), v.to_str().unwrap().to_string()));
                        }
                    }
                    let unauthorized = headers
                        .get("authorization")
                        .is_some_and(|v| v.to_str().unwrap().ends_with("dead"));
                    let throttled = headers
                        .get("authorization")
                        .is_some_and(|v| v.to_str().unwrap().ends_with("busy"));
                    if unauthorized {
                        (axum::http::StatusCode::UNAUTHORIZED, "{}".to_string()).into_response()
                    } else if throttled {
                        (
                            axum::http::StatusCode::TOO_MANY_REQUESTS,
                            [(axum::http::header::RETRY_AFTER, "120")],
                            "{}".to_string(),
                        )
                            .into_response()
                    } else {
                        (axum::http::StatusCode::OK, PAYLOAD.to_string()).into_response()
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let usage = fetch_usage(
            &client,
            &base,
            "at-live",
            Some("11111111-1111-4111-8111-111111111111"),
        )
        .await
        .unwrap();
        assert!(usage.five_hour.is_some());
        let s = seen.lock().unwrap().clone();
        assert!(s.contains(&("authorization".to_string(), "Bearer at-live".to_string())));
        assert!(s.contains(&(
            "chatgpt-account-id".to_string(),
            "11111111-1111-4111-8111-111111111111".to_string()
        )));
        let err = fetch_usage(&client, &base, "at-dead", None)
            .await
            .unwrap_err();
        assert_eq!(
            err.status,
            Some(401),
            "a 401 must surface as such so the manager forces a refresh"
        );
        let account_ids: Vec<String> = seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(name, _)| name == "chatgpt-account-id")
            .map(|(_, value)| value.clone())
            .collect();
        assert_eq!(
            account_ids,
            ["11111111-1111-4111-8111-111111111111"],
            "only the first call named an account: a row with none sends no chatgpt-account-id"
        );

        let err = fetch_usage(&client, &base, "at-busy", None)
            .await
            .unwrap_err();
        assert_eq!(err.status, Some(429));
        assert_eq!(
            err.retry_after_secs,
            Some(120),
            "a 429's retry-after must reach the manager, which backs the probe off by it"
        );
    }
}
