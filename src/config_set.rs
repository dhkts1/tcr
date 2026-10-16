//! `tcr config set <key> <value>`: one rotation or limit setting, parsed once
//! into a typed [`Setting`] and written as one top-level key.
//!
//! # Who writes the file
//!
//! With a proxy running, the CLI does not write the file itself: it sends the
//! setting to the proxy's `/_tcr/config` route (`crate::proxy::CONFIG_PATH`),
//! and the proxy writes it through [`save_setting`] under its own config-write
//! lock, the one its token rotations and `tcr disable` writes already hold. The
//! two writers can then never interleave a read-modify-write and drop each
//! other's change. With no proxy, the CLI calls [`save_setting`] itself, the
//! same function, so the bytes on disk do not depend on which process wrote
//! them.
//!
//! # What changes at once, and what waits
//!
//! Nothing here changes the running proxy's behaviour. Every one of these keys
//! is a boot-time read (`CLAUDE.md`, "`tcr status` reports the RUNNING
//! process's view"), and three of them (`http1Only`, `sessionAffinity`,
//! `usageRetentionDays`) are read out of the manager's in-memory `Config` on
//! demand, so changing that copy would switch them on halfway: the affinity
//! check per request would flip while the pin flusher started at boot would
//! not. The proxy therefore persists the key and leaves its memory alone, and
//! `tcr config show --json` reports both: the saved value, and under `running`
//! the one the proxy is using. A key whose two differ is waiting for a restart.
//!
//! # The ranges
//!
//! Each value is refused with a sentence, and nothing written, where the
//! server would otherwise normalise it into something the operator did not
//! type: a reserve outside `[0, 0.5]` (the manager clamps it,
//! `Config::effective_control_reserve`), a throttle spacing or burst of `0`
//! (`ThrottleConfig::effective_min_spacing` reads `0` as off, `effective_burst`
//! as `1`), a pacing cap of `0` (`PacingConfig::effective_max_in_flight` reads
//! it as no cap). The two thresholds are shares of quota, so above `0` and at
//! most `1`; the server applies no range to them, and a `0` would gate every
//! account at once.

use std::fmt;
use std::path::Path;

use serde_json::{Map, Value};

use crate::config::{self, ConfigError};

/// The eleven top-level keys `tcr config set` writes, each spelled once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingKey {
    SwitchThreshold,
    ControlReserve,
    FableWeeklyThreshold,
    ResetUrgencyTierHours,
    SessionAffinity,
    ControlPooled,
    AccountThrottle,
    FleetThrottle,
    Pacing,
    UsageRetentionDays,
    Http1Only,
}

impl SettingKey {
    /// Every key, in the order `tcr config show` prints them.
    pub const ALL: [SettingKey; 11] = [
        Self::SwitchThreshold,
        Self::ControlReserve,
        Self::FableWeeklyThreshold,
        Self::ResetUrgencyTierHours,
        Self::SessionAffinity,
        Self::ControlPooled,
        Self::AccountThrottle,
        Self::FleetThrottle,
        Self::Pacing,
        Self::UsageRetentionDays,
        Self::Http1Only,
    ];

    /// The key as the config file spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SwitchThreshold => "switchThreshold",
            Self::ControlReserve => "controlReserve",
            Self::FableWeeklyThreshold => "fableWeeklyThreshold",
            Self::ResetUrgencyTierHours => "resetUrgencyTierHours",
            Self::SessionAffinity => "sessionAffinity",
            Self::ControlPooled => "controlPooled",
            Self::AccountThrottle => "accountThrottle",
            Self::FleetThrottle => "fleetThrottle",
            Self::Pacing => "pacing",
            Self::UsageRetentionDays => "usageRetentionDays",
            Self::Http1Only => "http1Only",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|key| key.as_str() == name)
    }

    /// What the value is, said the way a refusal says it.
    fn expects(self) -> &'static str {
        match self {
            Self::SwitchThreshold | Self::FableWeeklyThreshold => {
                "a share of quota above 0 and at most 100%, as 90% or 0.9"
            }
            Self::ControlReserve => "a share of quota from 0 to 50%, as 5% or 0.05",
            Self::ResetUrgencyTierHours => "a whole number of hours, 0 to turn the tier off",
            Self::UsageRetentionDays => "a whole number of days",
            Self::SessionAffinity | Self::ControlPooled | Self::Http1Only => "on or off",
            Self::AccountThrottle | Self::FleetThrottle => {
                "two whole numbers of at least 1, the milliseconds between sends and the burst, or off"
            }
            Self::Pacing => "a count of at least 1, the most requests one account may have in flight, or off",
        }
    }
}

impl fmt::Display for SettingKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One GCRA bucket's two numbers, both at least `1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Throttle {
    pub min_spacing_ms: u64,
    pub burst: u32,
}

/// One setting, parsed. Built only by [`Setting::parse`], so every value
/// here is one the server would apply exactly as written.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Setting {
    SwitchThreshold(f64),
    ControlReserve(f64),
    /// `None` removes the override, so the Fable weekly window follows
    /// `switchThreshold` again.
    FableWeeklyThreshold(Option<f64>),
    ResetUrgencyTierHours(u32),
    SessionAffinity(bool),
    ControlPooled(bool),
    /// `None` is the bucket turned off (`{}` in the file).
    AccountThrottle(Option<Throttle>),
    FleetThrottle(Option<Throttle>),
    /// `None` is pacing off; `Some(n)` caps in-flight requests per account.
    Pacing(Option<u32>),
    UsageRetentionDays(u32),
    Http1Only(bool),
}

/// Why a value was refused: one sentence an operator can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingError(String);

impl fmt::Display for SettingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SettingError {}

impl Setting {
    /// Parse `value` for the key named `key`. `value` is the words after the
    /// key, joined by spaces, so a throttle is `"350 24"`.
    pub fn parse(key: &str, value: &str) -> Result<Self, SettingError> {
        let Some(key) = SettingKey::from_name(key) else {
            let names: Vec<&str> = SettingKey::ALL.iter().map(|k| k.as_str()).collect();
            return Err(SettingError(format!(
                "'{key}' is not a setting `tcr config set` changes. It changes {}.",
                names.join(", ")
            )));
        };
        let value = value.trim();
        let refuse = || {
            SettingError(format!(
                "{key} takes {}, not '{value}'. Nothing was changed.",
                key.expects()
            ))
        };
        let setting = match key {
            SettingKey::SwitchThreshold => {
                Self::SwitchThreshold(share(value, false, 1.0).ok_or_else(refuse)?)
            }
            SettingKey::ControlReserve => {
                Self::ControlReserve(share(value, true, 0.5).ok_or_else(refuse)?)
            }
            SettingKey::FableWeeklyThreshold => {
                if value == "default" {
                    Self::FableWeeklyThreshold(None)
                } else {
                    Self::FableWeeklyThreshold(Some(share(value, false, 1.0).ok_or_else(refuse)?))
                }
            }
            SettingKey::ResetUrgencyTierHours => {
                Self::ResetUrgencyTierHours(value.parse().map_err(|_| refuse())?)
            }
            SettingKey::UsageRetentionDays => {
                Self::UsageRetentionDays(value.parse().map_err(|_| refuse())?)
            }
            SettingKey::SessionAffinity => Self::SessionAffinity(switch(value).ok_or_else(refuse)?),
            SettingKey::ControlPooled => Self::ControlPooled(switch(value).ok_or_else(refuse)?),
            SettingKey::Http1Only => Self::Http1Only(switch(value).ok_or_else(refuse)?),
            SettingKey::AccountThrottle => {
                Self::AccountThrottle(throttle(value).ok_or_else(refuse)?)
            }
            SettingKey::FleetThrottle => Self::FleetThrottle(throttle(value).ok_or_else(refuse)?),
            SettingKey::Pacing => {
                if value == "off" {
                    Self::Pacing(None)
                } else {
                    match value.parse::<u32>() {
                        Ok(cap) if cap >= 1 => Self::Pacing(Some(cap)),
                        _ => return Err(refuse()),
                    }
                }
            }
        };
        Ok(setting)
    }

    pub fn key(&self) -> SettingKey {
        match self {
            Self::SwitchThreshold(_) => SettingKey::SwitchThreshold,
            Self::ControlReserve(_) => SettingKey::ControlReserve,
            Self::FableWeeklyThreshold(_) => SettingKey::FableWeeklyThreshold,
            Self::ResetUrgencyTierHours(_) => SettingKey::ResetUrgencyTierHours,
            Self::SessionAffinity(_) => SettingKey::SessionAffinity,
            Self::ControlPooled(_) => SettingKey::ControlPooled,
            Self::AccountThrottle(_) => SettingKey::AccountThrottle,
            Self::FleetThrottle(_) => SettingKey::FleetThrottle,
            Self::Pacing(_) => SettingKey::Pacing,
            Self::UsageRetentionDays(_) => SettingKey::UsageRetentionDays,
            Self::Http1Only(_) => SettingKey::Http1Only,
        }
    }

    /// The value in the words [`Self::parse`] reads back to this same
    /// setting, which is what the CLI sends the proxy.
    pub fn value_text(&self) -> String {
        let on_off = |b: bool| if b { "on" } else { "off" }.to_string();
        let bucket = |t: &Option<Throttle>| match t {
            None => "off".to_string(),
            Some(t) => format!("{} {}", t.min_spacing_ms, t.burst),
        };
        match self {
            Self::SwitchThreshold(f) | Self::ControlReserve(f) => f.to_string(),
            Self::FableWeeklyThreshold(None) => "default".into(),
            Self::FableWeeklyThreshold(Some(f)) => f.to_string(),
            Self::ResetUrgencyTierHours(n) | Self::UsageRetentionDays(n) => n.to_string(),
            Self::SessionAffinity(b) | Self::ControlPooled(b) | Self::Http1Only(b) => on_off(*b),
            Self::AccountThrottle(t) | Self::FleetThrottle(t) => bucket(t),
            Self::Pacing(None) => "off".into(),
            Self::Pacing(Some(n)) => n.to_string(),
        }
    }

    /// The value as `tcr config set` prints it back: `90%`, `1 per 350 ms,
    /// bursts of 24`, `off`.
    pub fn value_words(&self) -> String {
        let percent = |f: f64| format!("{}%", (f * 1000.0).round() / 10.0);
        match self {
            Self::SwitchThreshold(f) | Self::ControlReserve(f) => percent(*f),
            Self::FableWeeklyThreshold(None) => "the switch threshold".into(),
            Self::FableWeeklyThreshold(Some(f)) => percent(*f),
            Self::ResetUrgencyTierHours(0) => "off".into(),
            Self::ResetUrgencyTierHours(n) => format!("{n} h"),
            Self::UsageRetentionDays(n) => format!("{n} days"),
            Self::AccountThrottle(Some(t)) | Self::FleetThrottle(Some(t)) => {
                format!("1 per {} ms, bursts of {}", t.min_spacing_ms, t.burst)
            }
            Self::Pacing(Some(n)) => format!("at most {n} in flight per account"),
            other => other.value_text(),
        }
    }

    /// Write this setting into `doc`, the config file's raw JSON object.
    /// Only this setting's key is touched.
    fn apply(&self, doc: &mut Map<String, Value>) {
        let key = self.key().as_str().to_string();
        let number = |f: f64| serde_json::Number::from_f64(f).map(Value::Number);
        let bucket = |t: &Option<Throttle>| match t {
            None => Value::Object(Map::new()),
            Some(t) => serde_json::json!({ "minSpacingMs": t.min_spacing_ms, "burst": t.burst }),
        };
        let value = match self {
            // `parse` refuses a non-finite number, so `from_f64` always
            // answers; the `None` arm removes nothing it should not.
            Self::SwitchThreshold(f) | Self::ControlReserve(f) => number(*f),
            Self::FableWeeklyThreshold(f) => f.and_then(number),
            Self::ResetUrgencyTierHours(n) | Self::UsageRetentionDays(n) => Some(Value::from(*n)),
            Self::SessionAffinity(b) | Self::ControlPooled(b) | Self::Http1Only(b) => {
                Some(Value::Bool(*b))
            }
            Self::AccountThrottle(t) | Self::FleetThrottle(t) => Some(bucket(t)),
            // The cap is pacing's one knob here; a `minSpacingMs` already in
            // the file is the operator's and stays. Off removes the key,
            // which is pacing's shipped default.
            Self::Pacing(Some(cap)) => {
                let mut pacing = match doc.get(&key) {
                    Some(Value::Object(existing)) => existing.clone(),
                    _ => Map::new(),
                };
                pacing.insert("maxInFlightPerAccount".into(), Value::from(*cap));
                Some(Value::Object(pacing))
            }
            Self::Pacing(None) => None,
        };
        match value {
            Some(value) => {
                doc.insert(key, value);
            }
            None => {
                doc.remove(&key);
            }
        }
    }
}

/// `90%` or `0.9`, finite, above `0` (or at `0` when `zero_ok`) and at most
/// `max`.
fn share(value: &str, zero_ok: bool, max: f64) -> Option<f64> {
    let fraction = match value.strip_suffix('%') {
        Some(percent) => percent.trim().parse::<f64>().ok()? / 100.0,
        None => value.parse::<f64>().ok()?,
    };
    let above_floor = if zero_ok {
        fraction >= 0.0
    } else {
        fraction > 0.0
    };
    (fraction.is_finite() && above_floor && fraction <= max).then_some(fraction)
}

fn switch(value: &str) -> Option<bool> {
    match value {
        "on" | "true" => Some(true),
        "off" | "false" => Some(false),
        _ => None,
    }
}

/// `off`, or `<ms> <burst>` with both at least `1`.
fn throttle(value: &str) -> Option<Option<Throttle>> {
    if value == "off" {
        return Some(None);
    }
    let mut words = value.split_whitespace();
    let (Some(spacing), Some(burst), None) = (words.next(), words.next(), words.next()) else {
        return None;
    };
    let min_spacing_ms: u64 = spacing.parse().ok()?;
    let burst: u32 = burst.parse().ok()?;
    (min_spacing_ms >= 1 && burst >= 1).then_some(Some(Throttle {
        min_spacing_ms,
        burst,
    }))
}

/// What [`save_setting`] did to the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingWrite {
    /// The key changed and the file was rewritten.
    Updated,
    /// The file already said this; nothing was written.
    Unchanged,
}

/// Write `setting` into the config file at `path`, leaving every other key
/// exactly as it is: a read-modify-write of the raw JSON document, the shape
/// `config::save_control_account` uses and for its reason (a typed `Config`
/// round trip would write every serde default back into the file).
pub fn save_setting(path: &Path, setting: &Setting) -> Result<SettingWrite, ConfigError> {
    let mut doc = config::read_document(path)?;
    let before = doc.clone();
    setting.apply(&mut doc);
    if doc == before {
        return Ok(SettingWrite::Unchanged);
    }
    config::write_atomic(path, &serde_json::to_string_pretty(&doc)?)?;
    Ok(SettingWrite::Updated)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(key: &str, value: &str) -> Setting {
        Setting::parse(key, value).unwrap_or_else(|e| panic!("{key} {value}: {e}"))
    }

    #[test]
    fn every_key_parses_its_documented_forms() {
        assert_eq!(
            parsed("switchThreshold", "90%"),
            Setting::SwitchThreshold(0.9)
        );
        assert_eq!(
            parsed("switchThreshold", "0.9"),
            Setting::SwitchThreshold(0.9)
        );
        assert_eq!(
            parsed("switchThreshold", "100%"),
            Setting::SwitchThreshold(1.0)
        );
        assert_eq!(parsed("controlReserve", "0"), Setting::ControlReserve(0.0));
        assert_eq!(
            parsed("controlReserve", "50%"),
            Setting::ControlReserve(0.5)
        );
        assert_eq!(
            parsed("fableWeeklyThreshold", "default"),
            Setting::FableWeeklyThreshold(None)
        );
        assert_eq!(
            parsed("fableWeeklyThreshold", "1"),
            Setting::FableWeeklyThreshold(Some(1.0))
        );
        assert_eq!(
            parsed("resetUrgencyTierHours", "0"),
            Setting::ResetUrgencyTierHours(0)
        );
        assert_eq!(
            parsed("sessionAffinity", "off"),
            Setting::SessionAffinity(false)
        );
        assert_eq!(
            parsed("controlPooled", "true"),
            Setting::ControlPooled(true)
        );
        assert_eq!(parsed("http1Only", "on"), Setting::Http1Only(true));
        assert_eq!(
            parsed("accountThrottle", "350 24"),
            Setting::AccountThrottle(Some(Throttle {
                min_spacing_ms: 350,
                burst: 24
            }))
        );
        assert_eq!(parsed("fleetThrottle", "off"), Setting::FleetThrottle(None));
        assert_eq!(parsed("pacing", "4"), Setting::Pacing(Some(4)));
        assert_eq!(parsed("pacing", "off"), Setting::Pacing(None));
        assert_eq!(
            parsed("usageRetentionDays", "30"),
            Setting::UsageRetentionDays(30)
        );
    }

    /// The text the CLI sends the proxy parses back to the same setting, so
    /// the proxy applies exactly what the CLI validated.
    #[test]
    fn value_text_round_trips_through_parse() {
        for (key, value) in [
            ("switchThreshold", "90%"),
            ("controlReserve", "5%"),
            ("fableWeeklyThreshold", "default"),
            ("fableWeeklyThreshold", "95.5%"),
            ("resetUrgencyTierHours", "48"),
            ("sessionAffinity", "off"),
            ("accountThrottle", "350 24"),
            ("fleetThrottle", "off"),
            ("pacing", "4"),
            ("pacing", "off"),
            ("usageRetentionDays", "7"),
            ("http1Only", "on"),
        ] {
            let setting = parsed(key, value);
            assert_eq!(
                Setting::parse(key, &setting.value_text()),
                Ok(setting),
                "{key} {value}"
            );
        }
    }

    /// Each value the server would quietly normalise, or that is not a value
    /// at all, is refused with a sentence that names the key.
    #[test]
    fn out_of_range_values_are_refused_with_a_sentence() {
        for (key, value) in [
            ("switchThreshold", "0"),
            ("switchThreshold", "101%"),
            ("switchThreshold", "1.5"),
            ("switchThreshold", "NaN"),
            ("switchThreshold", "ninety"),
            ("controlReserve", "0.6"),
            ("controlReserve", "-1%"),
            ("fableWeeklyThreshold", "0"),
            ("resetUrgencyTierHours", "-1"),
            ("resetUrgencyTierHours", "1.5"),
            ("usageRetentionDays", ""),
            ("sessionAffinity", "maybe"),
            ("accountThrottle", "350"),
            ("accountThrottle", "0 24"),
            ("accountThrottle", "350 0"),
            ("fleetThrottle", "100 32 1"),
            ("pacing", "0"),
            ("pacing", "-2"),
        ] {
            let err = Setting::parse(key, value).expect_err(&format!("{key} '{value}' is refused"));
            let text = err.to_string();
            assert!(
                text.starts_with(key) && text.contains("Nothing was changed."),
                "{key} '{value}': {text}"
            );
        }
        let err = Setting::parse("controlAccount", "alice@example.com").expect_err("not ours");
        assert!(err.to_string().contains("switchThreshold"), "{err}");
    }

    fn scratch(name: &str, body: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "tcr-config-set-{name}-{}-{}.json",
            std::process::id(),
            crate::now_ms()
        ));
        std::fs::write(&path, body).expect("write the scratch config");
        path
    }

    #[test]
    fn save_setting_writes_one_key_and_leaves_the_rest() {
        let path = scratch(
            "one-key",
            r#"{"proxy": {"port": 1}, "pacing": {"minSpacingMs": 200}, "fableWeeklyThreshold": 1.0, "someoneElses": [1, 2]}"#,
        );
        let write = save_setting(&path, &Setting::SwitchThreshold(0.9)).expect("saves");
        assert_eq!(write, SettingWrite::Updated);
        save_setting(&path, &Setting::Pacing(Some(4))).expect("saves");
        save_setting(&path, &Setting::FableWeeklyThreshold(None)).expect("saves");
        save_setting(&path, &Setting::AccountThrottle(None)).expect("saves");
        let doc: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("reads")).expect("json");
        assert_eq!(doc["switchThreshold"], serde_json::json!(0.9));
        assert_eq!(
            doc["pacing"],
            serde_json::json!({"minSpacingMs": 200, "maxInFlightPerAccount": 4}),
            "the operator's own spacing stays"
        );
        assert!(
            doc.get("fableWeeklyThreshold").is_none(),
            "default removes it"
        );
        assert_eq!(doc["accountThrottle"], serde_json::json!({}), "off is {{}}");
        assert_eq!(doc["someoneElses"], serde_json::json!([1, 2]));
        assert_eq!(doc["proxy"], serde_json::json!({"port": 1}));
        assert!(doc.get("fleetThrottle").is_none(), "no default is written");

        let before = std::fs::read_to_string(&path).expect("reads");
        assert_eq!(
            save_setting(&path, &Setting::SwitchThreshold(0.9)).expect("saves"),
            SettingWrite::Unchanged
        );
        assert_eq!(before, std::fs::read_to_string(&path).expect("reads"));
        std::fs::remove_file(&path).ok();
    }

    /// What `save_setting` writes is what the loader then reads, through the
    /// same effective-value functions the manager uses.
    #[test]
    fn a_saved_setting_loads_as_the_server_would_use_it() {
        let path = scratch("loads", r#"{"proxy": {"port": 1}}"#);
        for setting in [
            Setting::SwitchThreshold(0.9),
            Setting::ControlReserve(0.1),
            Setting::SessionAffinity(false),
            Setting::FleetThrottle(Some(Throttle {
                min_spacing_ms: 50,
                burst: 8,
            })),
            Setting::Pacing(Some(3)),
            Setting::UsageRetentionDays(30),
        ] {
            save_setting(&path, &setting).expect("saves");
        }
        let config = config::load(&path).expect("loads");
        let shown = crate::config_show::ConfigSettings::from_config(&config, &path);
        assert_eq!(shown.switch_threshold, 0.9);
        assert_eq!(shown.control_reserve, 0.1);
        assert!(!shown.session_affinity);
        assert_eq!(
            shown.fleet_throttle,
            Some(crate::config_show::ThrottleSetting {
                min_spacing_ms: 50,
                burst: 8
            })
        );
        assert_eq!(
            shown.pacing.and_then(|p| p.max_in_flight_per_account),
            Some(3)
        );
        assert_eq!(shown.usage_retention_days, 30);
        std::fs::remove_file(&path).ok();
    }
}
