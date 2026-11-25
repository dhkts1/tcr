//! Typed readers for the unmodelled top-level keys the manager reads out of
//! [`Config::extra`], each carrying its default and the reason for it. The
//! manager's methods of the same names call these; `config_keys.rs` lists every
//! key read here as `ReadFromExtra`.

use serde_json::Value;

use super::Config;

/// Default probe cadence when `quotaProbeSeconds` is absent from the config.
///
/// This is the CENTRE of a per-account random draw, not a fleet-wide period: the
/// background scheduler ([`crate::manager::freshness::schedule`]) draws each account's next probe
/// uniformly from `cadence +/- 30%` and gives each a random initial offset, so N
/// accounts never share an instant and a restart re-scatters them. Raised from
/// 75s to 300s together with that change — a synchronized 75s fleet sweep was
/// both the loudest and the most fingerprintable thing this proxy did, and the
/// bars do not need refreshing four times a minute.
pub(crate) const DEFAULT_PROBE_SECONDS: u64 = 300;

impl Config {
    /// Probe cadence in seconds, read from the unmodelled `quotaProbeSeconds`
    /// (default [`DEFAULT_PROBE_SECONDS`]). A value `<= 0`
    /// disables probing.
    pub fn probe_interval_seconds(&self) -> u64 {
        self.extra
            .get("quotaProbeSeconds")
            .and_then(Value::as_i64)
            .map_or(DEFAULT_PROBE_SECONDS, |v| v.max(0) as u64)
    }

    /// Keep-warm cadence in seconds, read from the unmodelled `warmupSeconds`.
    /// **Default 0 = OFF** (unlike the probe's 75) — keep-warm spends real
    /// quota, so it ships dark and is only ever running when explicitly
    /// enabled. A value `<= 0` disables it (no warm task is spawned).
    pub fn warmup_interval_seconds(&self) -> u64 {
        self.extra
            .get("warmupSeconds")
            .and_then(Value::as_i64)
            .map_or(0, |v| v.max(0) as u64)
    }

    /// Over-threshold revalidation-serve is ON by default; set top-level
    /// `"revalidationServe": false` to disable it (pure fall-through to a
    /// synthesized 429 when the whole fleet reads over the soft threshold).
    /// Same read pattern as [`Self::session_affinity_enabled`]. See
    /// `Manager::select_revalidation`.
    pub fn revalidation_serve_enabled(&self) -> bool {
        self.extra
            .get("revalidationServe")
            .and_then(Value::as_bool)
            .unwrap_or(true)
    }

    /// Whether load-balancing migration is enabled, read from the unmodelled
    /// top-level `loadBalanceMigration` (**default `false` — OFF**). Same read
    /// pattern as [`Self::session_affinity_enabled`].
    ///
    /// It ships OFF because that migration moves an ALREADY-WARM session to a
    /// cooler account purely to even out pinned-session counts, and Anthropic's
    /// prompt cache is per-account: every such move costs a full prompt-cache
    /// re-creation of the whole conversation prefix on the target. A session's
    /// account is chosen at START, or when its pin fails an ACCOUNT-level HARD
    /// gate (`Manager::account_hard_ok`) — never merely to balance load. Set
    /// `"loadBalanceMigration": true` to restore the balancing behaviour.
    pub fn load_balance_migration_enabled(&self) -> bool {
        self.extra
            .get("loadBalanceMigration")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    /// Whether every request's `cache_control` breakpoints are rewritten to the
    /// 1-hour window before forwarding (see [`crate::records::cache_ttl::extend_all_ttls`]),
    /// read from the unmodelled top-level `cacheTtlRewrite`. Same read pattern
    /// as [`Self::session_affinity_enabled`].
    ///
    /// **Default: on when every account is OAuth, off when any is an API key.**
    /// Measured 2026-10-06, a 1h write costs a subscription's 5-hour window exactly
    /// what a 5m write does and a read costs it nothing visible, so for OAuth
    /// accounts the longer window is free and saves the full re-write after every
    /// 5-60 minute idle gap. An API-key row never reaches this list
    /// (`config::load` quarantines it and live-add refuses it), so the only
    /// row that turns the default off is one whose `type` is some other string
    /// ([`crate::base::config::AccountType::Unrecognized`]): a static key tcr cannot
    /// classify, which may be billed per token at the published rates, where a
    /// 1h write is 2x input against 1.25x for 5m. There the rewrite would raise
    /// the bill 60% on every write, so a fleet with one such row forwards
    /// bodies as sent unless the operator says otherwise.
    /// The rewrite runs before an account is picked, so it cannot decide per
    /// account; the fleet's composition is the finest grain it has. An explicit
    /// `"cacheTtlRewrite": true` or `false` overrides the default either way.
    pub fn cache_ttl_rewrite_enabled(&self) -> bool {
        match self.extra.get("cacheTtlRewrite").and_then(Value::as_bool) {
            Some(explicit) => explicit,
            None => self.accounts.iter().all(|a| a.account_type.is_oauth()),
        }
    }

    /// Max DISTINCT destination accounts one session may be diverted to inside a
    /// single hold episode, read from the unmodelled top-level `divertBudget`.
    /// Same read pattern as [`Self::session_affinity_enabled`].
    ///
    /// **Default `0` = unlimited = today's behaviour byte-for-byte.** This is
    /// the kill switch: an unbounded budget never blocks a divert, so shipping
    /// it means every existing divert path is untouched until someone opts in
    /// with `"divertBudget": N`. It stays `0` rather than a small nonzero
    /// default because flipping it is a production-traffic decision (a nonzero
    /// budget can turn a would-have-served divert into a `None`, which is new
    /// client-visible behaviour) that must follow the live measurement, not
    /// accompany it — see the divert-budget design notes §4.6.
    pub fn divert_budget(&self) -> u32 {
        self.extra
            .get("divertBudget")
            .and_then(Value::as_u64)
            .map_or(0, |v| v.min(u64::from(u32::MAX)) as u32)
    }

    /// Whether the automatic spend of expiring resets runs, read from the
    /// unmodelled `autoUseExpiringResets` (**default `true`**, Gil, 2026-11-17).
    pub fn auto_reset_enabled(&self) -> bool {
        self.extra
            .get("autoUseExpiringResets")
            .and_then(Value::as_bool)
            .unwrap_or(true)
    }

    /// Whether `Noise`-classified traffic (`/api/event_logging*`,
    /// `/mcp-registry*` — see `manager::select::classify_request`)
    /// skips the PER-ORG GCRA, read from the unmodelled top-level
    /// `throttleExemptNoise`. **Default `false`** — every account-served request
    /// pays a slot. Same read pattern as [`Self::session_affinity_enabled`].
    ///
    /// Note this exempts ONE of the two buckets. Exempt traffic still pays the
    /// fleet ceiling, so it is never entirely unpaced.
    ///
    /// Ships OFF deliberately, but the reasoning changed when the throttle was
    /// split in two. The original argument was that freeing telemetry's ~32% of
    /// slots does not reduce upstream pressure, it *reallocates* it onto
    /// `/v1/messages` — true when ONE bucket was all there was, because every slot
    /// telemetry gave up was a slot inference immediately took.
    ///
    /// With a per-org bucket plus a fleet ceiling that is no longer the trade.
    /// Exempt traffic now skips only the PER-ORG bucket and still pays the fleet
    /// ceiling (see `Manager::throttle_send`), so total egress stays bounded
    /// while telemetry stops consuming an organization's burst budget — which is
    /// the budget an interactive turn is actually waiting on.
    ///
    /// The code default stays `false` regardless: this repo is public and other
    /// fleets may not have the account count to absorb it.
    pub fn throttle_exempt_noise_enabled(&self) -> bool {
        self.extra
            .get("throttleExemptNoise")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }
}
