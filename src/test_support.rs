//! The one home for test helpers, unit and integration alike: the `#[cfg(test)]` modules reach it
//! as `crate::test_support`, and `tests/` as `tcr::test_support` through the `test-support`
//! feature (see the self dev-dependency in `Cargo.toml`). Never compiled into a release build.
//!
//! - [`Account::fixture`] and [`Config::fixture`]: one `Account` and one `Config`, filled in with
//!   struct-update syntax. A test writes only the fields it changes
//!   (`Account { priority: Some(2), ..Account::fixture(n) }`), so a field added to either type is
//!   added here once instead of in every literal.
//! - [`stub_upstream`]: fake upstreams served from two shared loopback listeners per binary.
//! - [`loopback`]: a client that never goes through an ambient proxy, and a port nothing holds.
//! - [`log_capture`]: a tracing writer a test can read back.
//! - [`live_proxy`]: a real proxy served in-process, for code that dials `proxy.port`.
//! - [`child`]: a spawned `tcr` killed when its test ends, even if the test binary is killed.
//! - [`libtest`]: proof that a test binary re-run as a child ran the one test it was asked for.

// `clippy.toml`'s test exemption reaches only a `#[cfg(test)]` module, and this one is also
// compiled under the feature; a helper that cannot do its job fails the test that called it,
// with a message.
#![expect(
    clippy::expect_used,
    reason = "a test helper fails the calling test with a message; the ban is for production code"
)]

pub mod child;
pub mod libtest;
pub mod live_proxy;
pub mod log_capture;
pub mod loopback;
pub mod status_wire;
pub mod stub_upstream;

use std::collections::HashMap;

use crate::config::{
    Account, AccountType, Config, Egress, PacingConfig, ProxyConfig, ThrottleConfig,
    default_codex_upstream,
};

impl Account {
    /// A healthy Anthropic OAuth account: token `at-<name>`, refresh token `rt-<name>`, valid for
    /// another hour, priority 0, no groups, local egress.
    pub fn fixture(name: &str) -> Self {
        Self {
            name: name.to_string(),
            account_type: AccountType::OAuth,
            provider: Default::default(),
            account_uuid: None,
            org_uuid: None,
            org_name: None,
            access_token: format!("at-{name}"),
            refresh_token: Some(format!("rt-{name}")),
            expires_at: Some(crate::now_ms() + 3_600_000),
            priority: Some(0),
            switch_threshold: None,
            disabled: None,
            groups: None,
            organization_type: None,
            rate_limit_tier: None,
            seat_tier: None,
            egress: Egress::Local,
            egress_strict: false,
            codex_home: None,
            extra: serde_json::Map::new(),
        }
    }
}

impl Config {
    /// A config over `accounts` with the defaults a fresh install would load.
    pub fn fixture(accounts: Vec<Account>) -> Self {
        Self {
            quarantined_accounts: Vec::new(),
            migrated_legacy_throttle: false,
            renamed_accounts: Vec::new(),
            rename_write_error: None,
            proxy: ProxyConfig::default(),
            upstream: "https://api.anthropic.com".to_string(),
            codex_upstream: default_codex_upstream(),
            switch_threshold: 0.90,
            fable_weekly_threshold: None,
            pacing: PacingConfig::default(),
            account_throttle: ThrottleConfig::default(),
            fleet_throttle: ThrottleConfig::default(),
            lock_account: None,
            control_account: None,
            control_reserve: 0.05,
            control_pooled: false,
            control_identity: Default::default(),
            reset_urgency_tier_hours: 24,
            http1_only: false,
            accounts,
            group_settings: HashMap::new(),
            pricing: Default::default(),
            usage_retention_days: 90,
            extra: serde_json::Map::new(),
        }
    }
}
