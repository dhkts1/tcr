//! Test helpers shared by more than one of the binary's test files: a fake tool-env host and
//! environment, and a config account carrying group labels.

use std::path::PathBuf;

use tcr::base::config;

use crate::cmd::tool_env;

/// A launch host that reads only `env` (no process environment, no real
/// `HOME`) and names `/tmp/ca.pem` as tcr's CA.
pub(super) fn test_host(env: &dyn Fn(&str) -> Option<String>) -> tool_env::Host<'_> {
    tool_env::Host {
        env,
        own_ca: PathBuf::from("/tmp/ca.pem"),
    }
}

/// An inherited environment holding exactly `vars`.
pub(super) fn inherited(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
    let vars: std::collections::HashMap<String, String> = vars
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();
    move |name| vars.get(name).cloned()
}

/// What `cmd` will set (`Some`) or remove (`None`) in the child's environment.
pub(super) fn env_changes(
    cmd: &std::process::Command,
) -> std::collections::BTreeMap<String, Option<String>> {
    cmd.get_envs()
        .map(|(name, value)| {
            (
                name.to_string_lossy().into_owned(),
                value.map(|v| v.to_string_lossy().into_owned()),
            )
        })
        .collect()
}

pub(super) fn account_with_groups(name: &str, groups: Option<&[&str]>) -> config::Account {
    config::Account {
        name: name.to_string(),
        account_type: config::AccountType::OAuth,
        provider: Default::default(),
        account_uuid: None,
        org_uuid: None,
        org_name: None,
        access_token: format!("at-{name}"),
        refresh_token: None,
        expires_at: None,
        priority: None,
        switch_threshold: None,
        disabled: None,
        groups: groups.map(|gs| gs.iter().map(ToString::to_string).collect()),
        organization_type: None,
        rate_limit_tier: None,
        seat_tier: None,
        plan_display_name: None,
        egress: config::Egress::Local,
        egress_strict: false,
        codex_home: None,
        extra: serde_json::Map::new(),
    }
}
