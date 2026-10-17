//! Fixtures for unit tests: one `Account` and one `Config`, filled in with struct-update syntax.
//!
//! A test writes only the fields it changes (`Account { priority: Some(2), ..Account::fixture(n) }`),
//! so a field added to either type is added here once instead of in every literal.

pub(crate) mod stub_upstream;

use std::collections::HashMap;

use crate::config::{
    Account, AccountType, Config, Egress, PacingConfig, ProxyConfig, ThrottleConfig,
    default_codex_upstream,
};

impl Account {
    /// A healthy Anthropic OAuth account: token `at-<name>`, refresh token `rt-<name>`, valid for
    /// another hour, priority 0, no groups, local egress.
    pub(crate) fn fixture(name: &str) -> Self {
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
    pub(crate) fn fixture(accounts: Vec<Account>) -> Self {
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
