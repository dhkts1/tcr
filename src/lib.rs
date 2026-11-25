//! tcr (Team Capacity Router) — a lean single-user rotating Anthropic proxy.
//!
//! It reads `~/.config/tcr.json`, rotates across the configured OAuth accounts as
//! their quota fills, refreshes tokens on expiry, streams requests to Anthropic
//! unchanged, and surfaces a live TUI. See `DESIGN.md` for the full contract.

#![forbid(unsafe_code)]

pub mod base;
pub mod clients;
pub mod manager;
pub mod peer;
pub mod proxy;
pub mod records;
pub mod request_key;
pub mod status;
pub mod stores;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
pub mod verbs;

use time::OffsetDateTime;

/// Current wall-clock time in Unix milliseconds.
///
/// Epoch-ms is the unit used by the config's `expiresAt` field and by every
/// token-expiry comparison, so keeping one canonical helper avoids drift.
pub fn now_ms() -> i64 {
    (OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}
