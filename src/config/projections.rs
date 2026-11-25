//! The read-only views other modules build from a whole config: the settings
//! `tcr config show` reports, and the price table a request is priced with.

use std::path::Path;

use super::Config;
use crate::config_show::{ConfigSettings, PacingSetting, ThrottleSetting};
use crate::pricing::PricingTable;

impl Config {
    /// The settings a server started on this config would use, with every
    /// default applied: [`ConfigSettings`], built once so every consumer reads
    /// the same value.
    pub fn settings(&self, config_path: &Path) -> ConfigSettings {
        ConfigSettings {
            config_path: config_path.to_path_buf(),
            switch_threshold: self.switch_threshold,
            control_reserve: self.effective_control_reserve(),
            fable_weekly_threshold: self.effective_fable_weekly_threshold(),
            fable_weekly_threshold_follows: self.fable_weekly_threshold.is_none(),
            reset_urgency_tier_hours: self.reset_urgency_tier_hours,
            session_affinity: self.session_affinity_enabled(),
            control_account: self.control_account_name().map(str::to_owned),
            control_pooled: self.control_pooled,
            account_throttle: ThrottleSetting::effective(&self.account_throttle),
            fleet_throttle: ThrottleSetting::effective(&self.fleet_throttle),
            pacing: PacingSetting::effective(&self.pacing),
            usage_retention_days: self.usage_retention_days,
            http1_only: self.http1_only,
        }
    }

    /// The price table a request is priced with: the built-in table under this
    /// config's `pricing` overrides. The running proxy and `tcr wrap`'s report
    /// both build it here, so a request prices the same in either.
    pub fn pricing_table(&self) -> PricingTable {
        PricingTable::new(self.pricing.clone().into_iter().collect())
    }
}
