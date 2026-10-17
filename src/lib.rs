//! tcr (Team Capacity Router) — a lean single-user rotating Anthropic proxy.
//!
//! A drop-in replacement for the JS teamclaude proxy: it reads the same
//! `~/.config/teamclaude.json`, rotates across the configured OAuth accounts as
//! their quota fills, refreshes tokens on expiry, streams requests to Anthropic
//! unchanged, and surfaces a live TUI. See `DESIGN.md` for the full contract.

#![forbid(unsafe_code)]

pub mod account_uuid;
pub mod affinity;
pub mod bound_tokens;
pub mod build_info;
pub(crate) mod cache_file;
pub mod cache_ttl;
pub mod claude_code_login;
pub mod cli;
pub mod codex_cli_login;
pub mod codex_local;
pub mod codex_login;
pub mod codex_oauth;
pub mod codex_probe;
pub mod codex_route;
mod codex_token_store;
pub mod config;
pub mod config_keys;
pub mod config_set;
pub mod config_show;
pub mod control_identity;
pub mod demo;
pub mod doctor;
pub mod fallback;
pub mod file_log;
#[cfg(unix)]
pub mod handoff;
pub mod http_client;
pub mod identity;
pub mod legacy_names;
pub mod log_err;
pub mod manager;
pub mod migrate;
pub mod mint;
pub mod mitm;
pub mod model;
pub mod oauth;
pub mod paths;
pub mod peer;
pub mod pricing;
pub mod probe;
pub mod provider;
pub mod provider_guard;
pub mod proxy;
pub mod quota;
pub mod request_body;
pub mod request_key;
pub mod save_gate;
pub mod schedule;
pub mod server;
pub mod session_wire;
pub mod session_wire_persist;
pub mod setup;
pub mod singleton;
pub mod stats;
pub mod status;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
pub mod tui;
pub mod update;
pub mod usage;
pub mod wake;
pub mod warmer;
pub mod wrap;

use time::OffsetDateTime;

/// Current wall-clock time in Unix milliseconds.
///
/// Epoch-ms is the unit used by the config's `expiresAt` field and by every
/// token-expiry comparison, so keeping one canonical helper avoids drift.
pub fn now_ms() -> i64 {
    (OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}
