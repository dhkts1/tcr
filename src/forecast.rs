#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::UsageRecord;
    use time::OffsetDateTime;

    fn record(at: i64, family: &str, input: u64, output: u64) -> UsageRecord {
        UsageRecord::from_quota_input(
            at,
            Some(format!("claude-{family}-5")),
            None,
            input + 600,
            input / 4,
            input / 4,
            600,
            output,
        )
    }

    fn buckets(percent: f64, reset: i64) -> [Option<crate::probe::UsageBucket>; 3] {
        [Some(crate::probe::UsageBucket {
            utilization: Some((percent + 0.001) / 100.0),
            reset_at_ms: Some(reset),
        }); 3]
    }

    fn train(families: &[&str]) -> Calibration {
        let mut c = Calibration::default();
        let mut now = 1_000_000;
        let mut percent = 10.0;
        let mut scoped = 10.0;
        c.observe(buckets(percent, 10_000_000), now, false);
        for _ in 0..12 {
            for family in families {
                for (input, output) in [(100, 0), (0, 25)] {
                    now += 1000;
                    c.record(&record(now - 1, family, input, output));
                    percent += 1.0;
                    if *family == "fable" {
                        scoped += 1.0;
                    }
                    let mut ticks = buckets(percent, 10_000_000);
                    ticks[2] = buckets(scoped, 10_000_000)[2];
                    c.observe(ticks, now, false);
                }
            }
        }
        c
    }

    #[test]
    fn forecast_fits_every_window_and_family_without_charging_reads_or_reasoning_twice() {
        let c = train(&crate::model::FAMILIES);
        for window in 0..3 {
            for family in crate::model::FAMILIES {
                let scale = c.scale(window, family);
                if window == 2 && family != "fable" {
                    assert!(scale.is_none(), "scoped window fits only fable");
                    continue;
                }
                let scale = scale.expect("full-rank sufficient evidence");
                assert!((scale.tokens_per_percent - 100.0).abs() < 2.0, "{scale:?}");
                assert!((scale.output_weight - 4.0).abs() < 0.1, "{scale:?}");
            }
        }
        let mut codex = Calibration::default();
        let mut percent = 0.0;
        codex.observe(buckets(percent, 10_000_000), 0, false);
        for n in 1..=24 {
            let mut r = record(
                n * 1000 - 1,
                "unused",
                if n % 2 == 0 { 100 } else { 0 },
                if n % 2 == 0 { 0 } else { 25 },
            );
            r.provider = Provider::Codex;
            r.model = Some("gpt-example".into());
            r.reasoning = r.output;
            codex.record(&r);
            percent += 1.0;
            codex.observe(buckets(percent, 10_000_000), n * 1000, false);
        }
        assert!((codex.scale(0, "gpt-example").unwrap().output_weight - 4.0).abs() < 0.1);
    }

    #[test]
    fn forecast_stays_cold_for_rank_deficiency_and_bad_or_unaligned_evidence() {
        let mut c = Calibration::default();
        c.observe(buckets(0.0, 10_000_000), 0, false);
        for n in 1..=50 {
            c.record(&record(n * 1000 - 1, "fable", 100, 25));
            c.observe(buckets(n as f64 * 2.0, 10_000_000), n * 1000, false);
        }
        assert!(
            c.scale(0, "fable").is_none(),
            "collinear input/output cannot identify two weights"
        );
        for bad in [f64::NAN, f64::INFINITY, -1.0] {
            let mut c = train(&["fable"]);
            c.observe(buckets(bad, 10_000_000), 2_000_000, false);
            assert!(c.scale(0, "fable").is_none());
        }
        let mut c = train(&["fable"]);
        c.observe(buckets(80.0, 10_000_000), 2_000_000, false);
        assert!(
            c.scale(0, "fable").is_none(),
            "unexplained percent is not a fit"
        );
        let mut c = train(&["fable"]);
        c.observe(buckets(40.0, 10_000_000), 2_000_000, true);
        assert!(
            c.scale(0, "fable").is_none(),
            "unfinished overlap cannot bracket exact usage"
        );
        let mut c = train(&["fable"]);
        c.record(&record(0, "fable", 100, 0));
        assert!(
            c.scale(0, "fable").is_none(),
            "late records invalidate alignment"
        );
        let mut c = train(&["fable"]);
        let mut missing = record(2_000_000, "fable", 100, 0);
        missing.model = None;
        c.record(&missing);
        assert!(
            c.scale(0, "fable").is_none(),
            "unclassified tokens cannot disappear"
        );
    }

    #[test]
    fn forecast_never_pairs_across_reset_missing_or_decreasing_readings() {
        let mut c = Calibration::default();
        c.observe(buckets(10.0, 5000), 1000, false);
        c.record(&record(2000, "fable", 100, 0));
        c.observe(buckets(11.0, 20_000), 6000, false);
        assert_eq!(c.windows[0].samples.len(), 0);
        c.record(&record(7000, "fable", 100, 0));
        c.observe([None; 3], 8000, false);
        c.observe(buckets(12.0, 20_000), 9000, false);
        assert_eq!(c.windows[0].samples.len(), 0);
        c.record(&record(10_000, "fable", 100, 0));
        c.observe(buckets(1.0, 20_000), 11_000, false);
        assert_eq!(c.windows[0].samples.len(), 0);
    }

    #[test]
    fn forecast_profile_is_monday_first_normalized_shrunk_and_banded() {
        let start = OffsetDateTime::parse(
            "2026-09-28T00:00:00Z",
            &time::format_description::well_known::Rfc3339,
        )
        .unwrap()
        .unix_timestamp()
            * 1000;
        let mut h = History::new(start);
        for hour in 0..672 {
            let input = if hour % 168 == 0 { 200 } else { 100 };
            h.record(&record(start + hour * HOUR_MS + 1, "opus", input, 25));
        }
        let now = start + 672 * HOUR_MS;
        let e = h.estimates(now).remove(0);
        assert_eq!(e.history_hours, 672.0);
        assert!((e.profile.iter().sum::<f64>() - 168.0).abs() < 1e-8);
        assert!((e.profile[0] / e.profile[1] - 225.0 / 125.0).abs() < 1e-8);
        let level = e.level.get("opus").unwrap();
        let band = e.band.get("opus").unwrap();
        assert!(band.lower.input <= level.input && band.upper.input >= level.input);
        let mut thin = History::new(start);
        thin.record(&record(start + 1, "opus", 100, 25));
        assert!(
            thin.estimates(start + HOUR_MS - 1).is_empty(),
            "no full hour yet"
        );
        let e = thin.estimates(start + HOUR_MS).remove(0);
        assert!(e.band["opus"].upper.input >= 3.0 * e.level["opus"].input);
        assert_eq!(e.band["opus"].lower.input, 0.0);
        assert!(e.profile.iter().all(|p| (*p - 1.0).abs() < 1e-8));
    }
}

#[cfg(test)]
mod public_forecast_tests {
    use crate::{
        config::{Account, Config},
        manager::Manager,
    };

    #[test]
    fn forecast_learning_and_reads_add_zero_upstream_calls() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let mut config = Config::fixture(vec![Account::fixture("alice@example.com")]);
        config.upstream = format!("http://{address}");
        config.codex_upstream = format!("http://{address}");
        let manager = Manager::with_live_refresher(config, None);
        let now = time::OffsetDateTime::now_utc();
        for step in 0..30 {
            let at = now + time::Duration::seconds(step);
            let bucket = Some(crate::probe::UsageBucket {
                utilization: Some(step as f64 / 100.0),
                reset_at_ms: Some(crate::now_ms() + 3_600_000),
            });
            manager.apply_usage_at(
                0,
                &crate::probe::Usage {
                    five_hour: bucket,
                    seven_day: bucket,
                    seven_day_oi: bucket,
                    ..Default::default()
                },
                at,
            );
            manager.record_usage(
                0,
                crate::usage::UsageRecord::from_quota_input(
                    (at.unix_timestamp_nanos() / 1_000_000) as i64 + 1,
                    Some("claude-fable-5".into()),
                    None,
                    100,
                    0,
                    0,
                    0,
                    25,
                ),
            );
            assert!(crate::routing::document(&manager, at).is_some());
        }
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "learning/observing never calls upstream"
        );
        let _control = std::net::TcpStream::connect(address).unwrap();
        assert!(
            listener.accept().is_ok(),
            "the same listener detects a real connection"
        );
    }
}
