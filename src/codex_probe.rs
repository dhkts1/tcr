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
//! payload carried thirteen top-level keys this parser does not read (`model_usage`,
//! `credits`, `spend_control`, `rate_limit_upsell`, …). So on that plan a probe fills
//! `seven_day` and leaves `five_hour` absent, never a fabricated 0%: a `null` window is skipped
//! and an unknown key is ignored.

use serde_json::Value;

use crate::probe::{ProbeError, Usage, UsageBucket};

/// `limit_window_seconds` at or under this is the five-hour bucket; above it, the weekly one.
pub const CODEX_FIVE_HOUR_CUTOFF_SECONDS: i64 = 36_000;

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
/// the backend serves that bearer on.
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
pub fn usage_from_payload(v: &Value) -> Usage {
    let mut usage = Usage::default();
    let Some(rate_limit) = v.get("rate_limit").filter(|r| !r.is_null()) else {
        return usage;
    };
    let now_s = crate::now_ms() / 1000;
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
        let bucket = if weekly {
            &mut usage.seven_day
        } else {
            &mut usage.five_hour
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
    usage
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
    /// the thirteen top-level keys this parser does not read, every nested key as the probe
    /// printed it for the exhausted login. The probe printed a leaf's type, not its value
    /// (save `plan_type`, `allowed`, `limit_reached`, `used_percent` and
    /// `limit_window_seconds`), so each other leaf is a placeholder of that type and the three
    /// identity keys are fixtures. Of an array it printed only the length, so the two items
    /// of each are guesses. `rate_limit_upsell` holds a `reset_at` of its own, a later instant
    /// than the window's, which a parser that searched the payload for any `reset_at` could
    /// report instead.
    /// By length the primary slot is the weekly bucket; by position it would put a weekly
    /// number and a days-away reset into `five_hour`. In both states `five_hour` stays absent
    /// rather than a fabricated 0%, and nothing else is filled.
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
