//! `classify_group_miss` names the constraint that emptied a group.

use super::*;
use crate::config::{Account, PacingConfig};

fn account(name: &str, groups: &[&str], disabled: bool) -> AccountRuntime {
    AccountRuntime::from_config(
        &Account {
            refresh_token: None,
            disabled: disabled.then_some(true),
            groups: Some(groups.iter().map(ToString::to_string).collect()),
            ..Account::fixture(name)
        },
        false,
    )
}

fn classify(
    accounts: &[AccountRuntime],
    control_excluded: Option<usize>,
    pacing: &PacingConfig,
    group: &str,
) -> GroupMiss {
    let now = OffsetDateTime::now_utc();
    Manager::classify_group_miss(
        accounts,
        &HashSet::new(),
        control_excluded,
        0.95,
        None,
        pacing,
        now,
        odt_to_ms(now),
        false,
        crate::provider::Provider::Anthropic,
        group,
        &HashSet::new(),
        &HashSet::new(),
    )
}

/// The live defect, in one assertion: `research` holds exactly one account
/// and that account is the control one, which inference never picks. The
/// group is not busy and not paced — it is permanently unroutable, and the
/// reason must say so.
#[test]
fn a_group_holding_only_the_control_account_is_named_control_account_only() {
    let accounts = [
        account("gil@example.com", &["research"], false),
        account("worker@example.com", &[], false),
    ];
    let miss = classify(&accounts, Some(0), &PacingConfig::default(), "research");
    assert_eq!(miss, GroupMiss::OnlyControl, "{miss:?}");
    assert_eq!(miss.as_str(), "control-account-only");
    assert!(
        miss.explain().contains("control account"),
        "the sentence an operator reads must name the control account: {}",
        miss.explain()
    );
}

/// Same fleet, same group — but the request is NOT inference, so nothing
/// excluded the control account and the group is perfectly routable. Guards
/// against reporting `OnlyControl` from group shape alone.
#[test]
fn the_same_group_is_not_control_blocked_when_nothing_was_excluded() {
    let accounts = [
        account("gil@example.com", &["research"], false),
        account("worker@example.com", &[], false),
    ];
    assert_ne!(
        classify(&accounts, None, &PacingConfig::default(), "research"),
        GroupMiss::OnlyControl
    );
}

/// A label no account carries is its own cause — a typo or a label removed
/// from every account, not a busy group.
#[test]
fn a_label_no_account_carries_is_unknown() {
    let accounts = [account("worker@example.com", &["dev"], false)];
    assert_eq!(
        classify(&accounts, None, &PacingConfig::default(), "resarch"),
        GroupMiss::Unknown
    );
}

/// A second, non-control member exists, so the group is not control-blocked
/// — but it is disabled, which is a hard gate and not pacing.
#[test]
fn a_hard_gated_member_is_unavailable_not_paced() {
    let accounts = [
        account("gil@example.com", &["research"], false),
        account("worker@example.com", &["research"], true),
    ];
    assert_eq!(
        classify(&accounts, Some(0), &PacingConfig::default(), "research"),
        GroupMiss::AllGated
    );
}

/// The one case the old hardcoded message was right about: pacing is
/// configured, the member is healthy, and it is inside its min-spacing
/// window. Pacing keeps its name — the fix is not to stop saying "paced",
/// it is to stop saying it when pacing is off.
#[test]
fn a_healthy_member_inside_the_spacing_window_is_paced() {
    let pacing = PacingConfig {
        max_in_flight_per_account: None,
        min_spacing_ms: Some(60_000),
    };
    let mut accounts = [account("worker@example.com", &["research"], false)];
    accounts[0].last_served_ms = crate::now_ms();
    assert_eq!(
        classify(&accounts, None, &pacing, "research"),
        GroupMiss::Paced
    );
}

/// `account`, as a Codex account.
fn codex_account(name: &str, groups: &[&str]) -> AccountRuntime {
    let mut runtime = account(name, groups, false);
    runtime.provider = crate::provider::Provider::Codex;
    runtime
}

/// A Claude request to a group whose only member is a Codex account is not
/// a busy group: no Claude request will ever be served there, because Claude
/// and Codex accounts never serve each other's requests. The reason says
/// that, rather than "every account in the requested group is currently
/// unavailable", which reads as something time will fix.
#[test]
fn a_claude_request_to_a_codex_only_group_is_other_provider_only() {
    let accounts = [
        account("worker@example.com", &["dev"], false),
        codex_account("bob@example.com/codex", &["lab"]),
    ];
    let miss = classify(&accounts, None, &PacingConfig::default(), "lab");
    assert_eq!(miss, GroupMiss::OtherProviderOnly, "{miss:?}");
    assert_eq!(miss.as_str(), "other-provider-only");
    assert_eq!(
        miss.explain(),
        "the requested group has no account of this request's provider: Claude and Codex accounts never serve each other's requests"
    );
}

/// A group holding the excluded control account and a Codex account is,
/// to a Claude request, a group holding only the control account: the Codex
/// member is not a candidate, so the reason is the control exclusion.
#[test]
fn a_control_plus_codex_group_is_only_control() {
    let accounts = [
        account("gil@example.com", &["research"], false),
        codex_account("bob@example.com/codex", &["research"]),
        account("worker@example.com", &[], false),
    ];
    assert_eq!(
        classify(&accounts, Some(0), &PacingConfig::default(), "research"),
        GroupMiss::OnlyControl
    );
}

/// A row deleted from the file keeps its rotation slot and its old groups
/// until a restart. The narrowing is by provider only, so such a slot still
/// counts as a member of its own provider: a fleet with no Codex account,
/// whose group's only member was just deleted, keeps answering "unavailable"
/// and never names a provider it does not have.
#[test]
fn a_group_holding_only_a_removed_claude_slot_is_all_gated() {
    let mut accounts = [
        account("gone@example.com", &["lab"], true),
        account("worker@example.com", &[], false),
    ];
    accounts[0].removed = true;
    assert_eq!(
        classify(&accounts, None, &PacingConfig::default(), "lab"),
        GroupMiss::AllGated
    );
}
