//! claude-swap's `list --json` schema v1, projected from tcr's status rows.
//!
//! CodexBar reads several Claude accounts at once from claude-swap: it runs
//! `cswap --list --json` and parses the object below with its
//! `ClaudeSwapListParser.parse` (`ClaudeSwapAccountList.swift` in CodexBar,
//! read at `5a40ac4`). `tcr list --json` prints the same object, so CodexBar
//! can show tcr's accounts with no change on its side; `docs/cli.md`, under
//! `tcr list`, has the three-line shim that points CodexBar at it.
//!
//! The parser rejects the WHOLE payload on any of these, so each one is a
//! constant or a type here rather than a value that could drift:
//!
//! - `schemaVersion` must be the integer `1` ([`SCHEMA_VERSION`]).
//! - `activeAccountNumber` must be present, and be a positive slot or `null`.
//!   tcr pins per session and has no one active account, so it is always
//!   `null`, and every row's `active` is `false` to agree with it.
//! - `supportsAccountSwitching`, when present, must be a boolean. tcr routes per
//!   request and has nothing to switch, so it is always `false`, which also hides
//!   CodexBar's Switch and Re-authenticate buttons.
//! - every row's `number` must be positive and unique: it is the 1-based
//!   position of the row in the slice handed to [`from_rows`], and the caller
//!   hands rows over in the config file's order.
//! - a present window must carry a finite `pct` and, if any, a parseable
//!   `resetsAt`. A window whose reading is unknown is omitted, never sent as
//!   `null` or `0`.
//!
//! What this leaves out on purpose: `alias` (tcr has none), `usage.spend` (tcr's
//! cost figure is a list-price equivalent with no limit, and CodexBar's money bar
//! needs one), and `lastGoodUsage` (tcr keeps no quota reading on disk). An API-key
//! account reads `unavailable`, not `api_key`, because [`AccountStatusRow`] does not
//! say which kind of account a row is.

use serde::de::IntoDeserializer;
use serde::de::value::{Error as TokenError, StrDeserializer};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::AccountStatusRow;

/// The only schema version CodexBar accepts.
pub const SCHEMA_VERSION: u32 = 1;

/// The `usage.scoped[]` name the Fable weekly window (`sevenDayOi`) is shown under.
pub const FABLE_SCOPE_NAME: &str = "Fable";

/// The whole `list --json` document.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListV1 {
    pub schema_version: u32,
    /// Always `None`, and serialized as `null` rather than omitted: CodexBar
    /// rejects a payload where the key is missing.
    pub active_account_number: Option<usize>,
    pub supports_account_switching: bool,
    pub accounts: Vec<AccountV1>,
}

/// One account row.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountV1 {
    /// 1-based slot: the row's position in the config file's `accounts` list.
    pub number: usize,
    pub email: String,
    /// The org's display name, or `""` when tcr never learned it.
    pub organization_name: String,
    pub active: bool,
    pub usage_status: UsageStatus,
    /// When the bars were last read, ISO-8601 UTC: a successful probe or the
    /// quota headers of a served response, whichever is newer
    /// (`quotaUpdatedAtMs`). Not `lastProbeMs`, because a failed probe stamps
    /// that while reading nothing, so a broken probe over hours-old bars would
    /// look fresh. Omitted when tcr has no reading time, which means unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage_fetched_at: Option<String>,
    /// Out of tcr's rotation: disabled by hand, or in a parked group.
    pub disabled: bool,
    pub usage: UsageV1,
}

/// A row's quota windows. Each is omitted when its reading is unknown.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageV1 {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub five_hour: Option<WindowV1>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seven_day: Option<WindowV1>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub scoped: Vec<ScopedWindowV1>,
}

/// One quota window: percent used (0 to 100) and when it resets.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowV1 {
    pub pct: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<String>,
}

/// A named window under `usage.scoped`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScopedWindowV1 {
    pub name: String,
    #[serde(flatten)]
    pub window: WindowV1,
}

/// The `usageStatus` values tcr emits, spelled the way CodexBar matches them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageStatus {
    /// The live server's last quota probe for this account succeeded.
    Ok,
    /// tcr gates the account on a login: its credential needs a human.
    ReloginRequired,
    /// No current reading: no server answered, the probe never ran or failed,
    /// or the row carried a gate this build does not know.
    Unavailable,
}

/// The handled failures `tcr list --json` reports in the error envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// The config file exists but could not be read or parsed.
    ConfigUnreadable,
    /// The document could not be serialized.
    RenderFailed,
}

impl ErrorKind {
    /// The `error.type` string CodexBar shows as "claude-swap reported <type>".
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ConfigUnreadable => "config_unreadable",
            Self::RenderFailed => "render_failed",
        }
    }
}

/// The schema-v1 error envelope, `{"schemaVersion":1,"error":{"type","message"}}`.
///
/// It goes to STDOUT with a non-zero exit: CodexBar parses stdout whatever the
/// exit code is, so an error that went only to stderr would read as "not JSON"
/// instead of as the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorV1 {
    pub kind: ErrorKind,
    pub message: String,
}

impl ErrorV1 {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// One line of JSON. Built as a `serde_json::Value`, whose `Display` cannot
    /// fail, so the error path itself has no error path.
    pub fn render(&self) -> String {
        serde_json::json!({
            "schemaVersion": SCHEMA_VERSION,
            "error": { "type": self.kind.as_str(), "message": self.message },
        })
        .to_string()
    }
}

/// Project status rows, already in config order, onto the schema-v1 document.
pub fn from_rows(rows: &[AccountStatusRow]) -> ListV1 {
    ListV1 {
        schema_version: SCHEMA_VERSION,
        active_account_number: None,
        supports_account_switching: false,
        accounts: rows
            .iter()
            .enumerate()
            .map(|(i, row)| project(i + 1, row))
            .collect(),
    }
}

fn project(number: usize, row: &AccountStatusRow) -> AccountV1 {
    let scoped = window(row.seven_day_oi, row.seven_day_oi_reset_at_ms)
        .map(|window| ScopedWindowV1 {
            name: FABLE_SCOPE_NAME.to_string(),
            window,
        })
        .into_iter()
        .collect();
    AccountV1 {
        number,
        email: email_of(&row.name).to_string(),
        organization_name: row.org_name.clone().unwrap_or_default(),
        active: false,
        usage_status: usage_status(row),
        usage_fetched_at: row.quota_updated_at_ms.and_then(iso_utc),
        disabled: row.disabled || !row.parked_groups.is_empty(),
        usage: UsageV1 {
            five_hour: window(row.five_hour, row.five_hour_reset_at_ms),
            seven_day: window(row.seven_day, row.seven_day_reset_at_ms),
            scoped,
        },
    }
}

/// The email half of an account name, before the `/org` suffix.
///
/// The same rule as `identity::email_of` in the main crate, which this crate
/// cannot depend on: `name` is `email` or `email/org-suffix`.
fn email_of(name: &str) -> &str {
    name.split_once('/').map_or(name, |(email, _)| email)
}

/// `source` on a row: which process measured it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Source {
    Live,
    Offline,
    #[serde(other)]
    Unrecognized,
}

/// `gate` on a row, the main crate's `GateReason` token.
///
/// Every token that build emits today is named, so that a NEW one (another
/// branch is adding `refused`) parses as [`Gate::Unrecognized`] and reads
/// `unavailable` rather than borrowing the probe's `ok`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Gate {
    Ok,
    Hold,
    FiveHour,
    SevenDay,
    FableWeekly,
    Standard,
    Login,
    Rejected,
    Disabled,
    Reserved,
    Parked,
    #[serde(other)]
    Unrecognized,
}

/// `probeStatus` on a row. Only a successful probe counts as a reading; every
/// other token (`never`, `error`, `timeout`, `rate-limited`, `upstream-down`,
/// or one added later) is the same "no current reading".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Probe {
    Ok,
    #[serde(other)]
    NoReading,
}

/// Parse a wire token into its enum once. Each enum above ends in a
/// `#[serde(other)]` variant, so any string parses; the fallback covers only
/// the impossible case and names the same "unknown" answer.
fn parse_token<T: for<'de> Deserialize<'de>>(raw: &str, unknown: T) -> T {
    let de: StrDeserializer<'_, TokenError> = raw.into_deserializer();
    T::deserialize(de).unwrap_or(unknown)
}

fn usage_status(row: &AccountStatusRow) -> UsageStatus {
    // Only the live server's numbers are a reading. An offline row was built
    // without probing (that is `tcr list`'s promise), so it has none.
    if parse_token(&row.source, Source::Unrecognized) != Source::Live {
        return UsageStatus::Unavailable;
    }
    match parse_token(&row.gate, Gate::Unrecognized) {
        Gate::Login => UsageStatus::ReloginRequired,
        Gate::Unrecognized => UsageStatus::Unavailable,
        Gate::Ok
        | Gate::Hold
        | Gate::FiveHour
        | Gate::SevenDay
        | Gate::FableWeekly
        | Gate::Standard
        | Gate::Rejected
        | Gate::Disabled
        | Gate::Reserved
        | Gate::Parked => match parse_token(&row.probe_status, Probe::NoReading) {
            Probe::Ok => UsageStatus::Ok,
            Probe::NoReading => UsageStatus::Unavailable,
        },
    }
}

/// A window from a fraction (0 to 1) and a reset in Unix ms. `None` when the
/// fraction is unknown or not finite: CodexBar refuses the whole payload over
/// one non-finite `pct`, and `serde_json` would write NaN as `null`.
fn window(fraction: Option<f64>, reset_at_ms: Option<i64>) -> Option<WindowV1> {
    let pct = fraction? * 100.0;
    if !pct.is_finite() {
        return None;
    }
    Some(WindowV1 {
        pct,
        resets_at: reset_at_ms.and_then(iso_utc),
    })
}

/// Unix ms to ISO-8601 UTC at whole seconds (`2026-01-01T00:00:00Z`), the
/// plainest form an ISO-8601 reader accepts. `None` for an instant outside
/// what `time` can represent, so a bad timestamp drops only its own field.
fn iso_utc(ms: i64) -> Option<String> {
    OffsetDateTime::from_unix_timestamp(ms.div_euclid(1000))
        .ok()?
        .format(&Rfc3339)
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_utc_writes_whole_seconds_in_utc() {
        assert_eq!(
            iso_utc(1_767_225_600_000).as_deref(),
            Some("2026-01-01T00:00:00Z")
        );
        assert_eq!(
            iso_utc(1_767_225_600_999).as_deref(),
            Some("2026-01-01T00:00:00Z"),
            "milliseconds are dropped, not rounded up"
        );
        assert_eq!(iso_utc(i64::MAX), None, "out of range drops the field");
    }

    #[test]
    fn a_non_finite_fraction_is_no_window() {
        assert_eq!(window(Some(f64::NAN), None), None);
        assert_eq!(window(None, Some(1_767_225_600_000)), None);
        assert_eq!(
            window(Some(0.5), None),
            Some(WindowV1 {
                pct: 50.0,
                resets_at: None
            })
        );
    }

    #[test]
    fn tokens_parse_once_and_unknown_ones_land_on_the_unknown_variant() {
        assert_eq!(parse_token("login", Gate::Unrecognized), Gate::Login);
        assert_eq!(parse_token("five-hour", Gate::Unrecognized), Gate::FiveHour);
        assert_eq!(
            parse_token("refused", Gate::Unrecognized),
            Gate::Unrecognized
        );
        assert_eq!(parse_token("ok", Probe::NoReading), Probe::Ok);
        assert_eq!(
            parse_token("rate-limited", Probe::NoReading),
            Probe::NoReading
        );
        assert_eq!(
            parse_token("offline", Source::Unrecognized),
            Source::Offline
        );
    }

    #[test]
    fn the_error_envelope_is_one_schema_v1_object() {
        let line = ErrorV1::new(ErrorKind::ConfigUnreadable, "bad json").render();
        let value: serde_json::Value = serde_json::from_str(&line).expect("valid JSON");
        assert_eq!(value["schemaVersion"], 1);
        assert_eq!(value["error"]["type"], "config_unreadable");
        assert_eq!(value["error"]["message"], "bad json");
        assert!(!line.contains('\n'), "one line");
    }
}
