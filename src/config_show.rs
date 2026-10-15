//! `tcr config show`: the rotation and limit settings a server started on this
//! config file would use, with every default applied.
//!
//! Offline: it reads the file and never asks a running proxy. That is the
//! point, and also its limit. Every value here except group settings is a
//! boot-time snapshot in the server (`CLAUDE.md`, "`tcr status` reports the
//! RUNNING process's view"), so a file edited since the proxy started shows
//! the value the NEXT start will use, which is what the Settings window's
//! "restart to apply" badge already says.
//!
//! TcrBar's Settings > Groups & Rotation reads `--json` (`ConfigSettings.swift`
//! in TcrBarCore decodes it), so the JSON keys are a contract with the panel.
//!
//! Each value is read through the same function the manager reads it through
//! (`Config::session_affinity_enabled`, `Config::effective_control_reserve`,
//! `Config::effective_fable_weekly_threshold`, the throttle and pacing
//! `effective_*` helpers), so this output and the running server cannot
//! disagree about what one file means.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::config::{Config, PacingConfig, ThrottleConfig};

/// One GCRA bucket as the send path runs it: `burst` sends at once after an
/// idle spell, then one per `min_spacing_ms`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThrottleSetting {
    pub min_spacing_ms: u64,
    pub burst: u32,
}

impl ThrottleSetting {
    /// `None` when the bucket is inert (no spacing, or a spacing of `0`),
    /// exactly when [`crate::manager::Manager::throttle_send`] skips it.
    pub fn effective(config: &ThrottleConfig) -> Option<Self> {
        config.effective_min_spacing().map(|min_spacing_ms| Self {
            min_spacing_ms,
            burst: config.effective_burst(),
        })
    }
}

/// Per-account pacing as selection applies it. Either knob may be absent on
/// its own; both absent is pacing off, which is `None` one level up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PacingSetting {
    pub max_in_flight_per_account: Option<u32>,
    pub min_spacing_ms: Option<u64>,
}

impl PacingSetting {
    /// `None` when [`PacingConfig::is_active`] is false. A cap of `0` reads as
    /// no cap, the same normalisation selection makes.
    pub fn effective(config: &PacingConfig) -> Option<Self> {
        config.is_active().then(|| Self {
            max_in_flight_per_account: config.effective_max_in_flight(),
            min_spacing_ms: config.min_spacing_ms,
        })
    }
}

/// The `tcr config show --json` document. Fractions stay fractions (`0.95`),
/// as the file writes them; the reader formats.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigSettings {
    pub config_path: PathBuf,
    pub switch_threshold: f64,
    pub control_reserve: f64,
    pub fable_weekly_threshold: f64,
    pub reset_urgency_tier_hours: u32,
    pub session_affinity: bool,
    /// The control account's name, or `null` when none is set.
    pub control_account: Option<String>,
    pub control_pooled: bool,
    /// `null` when the per-organization throttle is off.
    pub account_throttle: Option<ThrottleSetting>,
    /// `null` when the per-provider fleet ceiling is off.
    pub fleet_throttle: Option<ThrottleSetting>,
    /// `null` when pacing is off, the default.
    pub pacing: Option<PacingSetting>,
    pub usage_retention_days: u32,
    pub http1_only: bool,
}

impl ConfigSettings {
    /// Built once from a loaded config; every consumer reads this value.
    pub fn from_config(config: &Config, config_path: &Path) -> Self {
        Self {
            config_path: config_path.to_path_buf(),
            switch_threshold: config.switch_threshold,
            control_reserve: config.effective_control_reserve(),
            fable_weekly_threshold: config.effective_fable_weekly_threshold(),
            reset_urgency_tier_hours: config.reset_urgency_tier_hours,
            session_affinity: config.session_affinity_enabled(),
            control_account: config
                .control_account
                .as_ref()
                .map(|control| control.name().to_owned()),
            control_pooled: config.control_pooled,
            account_throttle: ThrottleSetting::effective(&config.account_throttle),
            fleet_throttle: ThrottleSetting::effective(&config.fleet_throttle),
            pacing: PacingSetting::effective(&config.pacing),
            usage_retention_days: config.usage_retention_days,
            http1_only: config.http1_only,
        }
    }

    /// The table `tcr config show` prints without `--json`: one `key: value`
    /// line per setting, keyed by the config file's own key names.
    pub fn render_text(&self) -> String {
        let rows: [(&str, String); 13] = [
            ("config", self.config_path.display().to_string()),
            ("switchThreshold", percent(self.switch_threshold)),
            ("controlReserve", percent(self.control_reserve)),
            ("fableWeeklyThreshold", percent(self.fable_weekly_threshold)),
            (
                "resetUrgencyTierHours",
                hours(self.reset_urgency_tier_hours),
            ),
            ("sessionAffinity", on_off(self.session_affinity)),
            (
                "controlAccount",
                self.control_account
                    .clone()
                    .unwrap_or_else(|| "none".into()),
            ),
            ("controlPooled", on_off(self.control_pooled)),
            ("accountThrottle", throttle(self.account_throttle)),
            ("fleetThrottle", throttle(self.fleet_throttle)),
            ("pacing", pacing(self.pacing)),
            ("usageRetentionDays", days(self.usage_retention_days)),
            ("http1Only", on_off(self.http1_only)),
        ];
        let width = rows.iter().map(|(key, _)| key.len()).max().unwrap_or(0);
        rows.iter()
            .map(|(key, value)| format!("{key:<width$}  {value}\n"))
            .collect()
    }
}

fn percent(fraction: f64) -> String {
    format!("{}%", (fraction * 100.0).round())
}

fn hours(value: u32) -> String {
    if value == 0 {
        "off (0)".into()
    } else {
        format!("{value} h")
    }
}

fn days(value: u32) -> String {
    format!("{value} days")
}

fn on_off(value: bool) -> String {
    if value { "on" } else { "off" }.into()
}

fn throttle(value: Option<ThrottleSetting>) -> String {
    match value {
        None => "off".into(),
        Some(t) => format!("1 per {} ms, bursts of {}", t.min_spacing_ms, t.burst),
    }
}

fn pacing(value: Option<PacingSetting>) -> String {
    let Some(p) = value else {
        return "off".into();
    };
    let mut parts = Vec::new();
    if let Some(cap) = p.max_in_flight_per_account {
        parts.push(format!("at most {cap} in flight per account"));
    }
    if let Some(spacing) = p.min_spacing_ms {
        parts.push(format!("{spacing} ms between picks of one account"));
    }
    parts.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(json: &str) -> ConfigSettings {
        let config: Config = serde_json::from_str(json).expect("a test config parses");
        ConfigSettings::from_config(&config, Path::new("/tmp/example.json"))
    }

    /// An empty file reports the shipped defaults, read off the same default
    /// functions the server uses, not copies of their numbers.
    #[test]
    fn an_empty_config_reports_the_shipped_defaults() {
        let s = settings("{}");
        assert_eq!(s.switch_threshold, 0.95);
        assert_eq!(s.control_reserve, 0.05);
        assert_eq!(
            s.fable_weekly_threshold, 0.95,
            "falls back to switchThreshold"
        );
        assert_eq!(s.reset_urgency_tier_hours, 24);
        assert!(s.session_affinity, "affinity defaults ON");
        assert_eq!(s.control_account, None);
        assert!(!s.control_pooled);
        assert_eq!(
            s.account_throttle,
            Some(ThrottleSetting {
                min_spacing_ms: 350,
                burst: 24
            })
        );
        assert_eq!(
            s.fleet_throttle,
            Some(ThrottleSetting {
                min_spacing_ms: 100,
                burst: 32
            })
        );
        assert_eq!(s.pacing, None, "pacing defaults OFF");
        assert_eq!(s.usage_retention_days, 90);
        assert!(!s.http1_only);
    }

    /// Every key set by hand is reported as the server would apply it:
    /// the reserve clamped, a `{}` throttle off, a zero cap dropped.
    #[test]
    fn set_values_are_reported_as_the_server_applies_them() {
        let s = settings(
            r#"{
                "switchThreshold": 0.9,
                "fableWeeklyThreshold": 1.0,
                "controlReserve": 0.8,
                "resetUrgencyTierHours": 0,
                "sessionAffinity": false,
                "controlAccount": "alice@example.com",
                "controlPooled": true,
                "accountThrottle": {},
                "fleetThrottle": {"minSpacingMs": 50},
                "pacing": {"maxInFlightPerAccount": 0, "minSpacingMs": 200},
                "usageRetentionDays": 30,
                "http1Only": true
            }"#,
        );
        assert_eq!(s.switch_threshold, 0.9);
        assert_eq!(s.fable_weekly_threshold, 1.0);
        assert_eq!(s.control_reserve, 0.5, "clamped to the manager's ceiling");
        assert_eq!(s.reset_urgency_tier_hours, 0);
        assert!(!s.session_affinity);
        assert_eq!(s.control_account.as_deref(), Some("alice@example.com"));
        assert!(s.control_pooled);
        assert_eq!(s.account_throttle, None, "an empty object turns it off");
        assert_eq!(
            s.fleet_throttle,
            Some(ThrottleSetting {
                min_spacing_ms: 50,
                burst: 1
            }),
            "an absent burst is strict spacing"
        );
        assert_eq!(
            s.pacing,
            Some(PacingSetting {
                max_in_flight_per_account: None,
                min_spacing_ms: Some(200)
            })
        );
        assert_eq!(s.usage_retention_days, 30);
        assert!(s.http1_only);
    }

    /// The control account in its identity-object shape reports its name.
    #[test]
    fn an_identity_control_account_reports_its_name() {
        let s = settings(
            r#"{"controlAccount": {"name": "alice@example.com", "accountUuid": "11111111-1111-1111-1111-111111111111"}}"#,
        );
        assert_eq!(s.control_account.as_deref(), Some("alice@example.com"));
    }

    /// The JSON keys TcrBar decodes, by name: a rename here breaks the panel.
    #[test]
    fn the_json_document_carries_the_panel_keys() {
        let value = serde_json::to_value(settings("{}")).expect("serializes");
        let object = value.as_object().expect("an object");
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "accountThrottle",
                "configPath",
                "controlAccount",
                "controlPooled",
                "controlReserve",
                "fableWeeklyThreshold",
                "fleetThrottle",
                "http1Only",
                "pacing",
                "resetUrgencyTierHours",
                "sessionAffinity",
                "switchThreshold",
                "usageRetentionDays",
            ]
        );
        assert_eq!(object["pacing"], serde_json::Value::Null);
        assert_eq!(object["controlAccount"], serde_json::Value::Null);
        assert_eq!(
            object["accountThrottle"],
            serde_json::json!({"minSpacingMs": 350, "burst": 24})
        );
    }

    #[test]
    fn the_text_table_says_each_value_in_words() {
        let text = settings(r#"{"pacing": {"maxInFlightPerAccount": 4}}"#).render_text();
        assert!(text.contains("switchThreshold        95%\n"), "{text}");
        assert!(
            text.contains("accountThrottle        1 per 350 ms, bursts of 24\n"),
            "{text}"
        );
        assert!(
            text.contains("pacing                 at most 4 in flight per account\n"),
            "{text}"
        );
        assert!(text.contains("controlAccount         none\n"), "{text}");
        assert!(text.contains("usageRetentionDays     90 days\n"), "{text}");
    }
}
