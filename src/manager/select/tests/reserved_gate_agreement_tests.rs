//! `eligible`, `account_gate` and `account_hard_ok` agree on the reserved gate.

use super::*;
use crate::config::{Account, PacingConfig};

fn account_runtime(groups: &[&str]) -> AccountRuntime {
    AccountRuntime::from_config(
        &Account {
            refresh_token: None,
            groups: Some(groups.iter().map(ToString::to_string).collect()),
            ..Account::fixture("probe")
        },
        false,
    )
}

/// Unrequested traffic (`group: None`, both `eligible`'s narrowing AND
/// reserved-check args): `eligible` and `account_gate` must agree with
/// `reserved_blocks` for every combination of an account's groups against
/// every reserved set — the account is otherwise perfectly healthy, so
/// RESERVED is the only thing that can hold it out.
#[test]
fn agree_on_unrequested_traffic_across_group_and_reserved_combinations() {
    let now = OffsetDateTime::now_utc();
    let now_ms = odt_to_ms(now);
    let pacing = PacingConfig::default();

    let group_combos: [&[&str]; 4] = [&[], &["codereview"], &["dev"], &["codereview", "dev"]];
    let reserved_combos: [&[&str]; 4] = [
        &[],
        &["codereview"],
        &["dev"],
        &["codereview", "dev", "burst"],
    ];

    for groups in group_combos {
        let account = account_runtime(groups);
        for reserved_list in reserved_combos {
            let reserved: HashSet<String> = reserved_list.iter().map(ToString::to_string).collect();
            let blocked = Manager::reserved_blocks(&account, None, &reserved);

            let elig = Manager::eligible(
                &account,
                0.90,
                None,
                &pacing,
                false,
                now,
                now_ms,
                false,
                crate::provider::Provider::Anthropic,
                None,
                None,
                &reserved,
                &HashSet::new(),
            );
            assert_eq!(
                elig, !blocked,
                "groups={groups:?} reserved={reserved_list:?}: eligible \
                 disagrees with reserved_blocks on an otherwise-healthy account"
            );

            let (gate, _) = Manager::account_gate(
                &account,
                0.90,
                None,
                now,
                now_ms,
                false,
                None,
                &reserved,
                &HashSet::new(),
            );
            assert_eq!(
                gate == GateReason::Reserved,
                blocked,
                "groups={groups:?} reserved={reserved_list:?}: account_gate \
                 disagrees with reserved_blocks"
            );

            let hard_ok = Manager::account_hard_ok(
                &account,
                now_ms,
                None,
                &reserved,
                &HashSet::new(),
                WarmWindow::DEFAULT,
            );
            assert_eq!(
                hard_ok, !blocked,
                "groups={groups:?} reserved={reserved_list:?}: account_hard_ok \
                 disagrees with reserved_blocks — a reserved pin would survive \
                 the hard-gate re-test"
            );
        }
    }
}

/// An explicit ask for one of the account's own groups always clears the
/// reserved gate in `eligible`, regardless of what else is reserved — the
/// spec's "session asked for group g -> eligible iff g in groups(A))"
/// branch, which never adds a second requirement on top of the match.
#[test]
fn an_explicit_matching_ask_is_never_reserved_blocked() {
    let now = OffsetDateTime::now_utc();
    let now_ms = odt_to_ms(now);
    let pacing = PacingConfig::default();
    let account = account_runtime(&["codereview", "dev"]);
    let reserved: HashSet<String> = ["codereview".to_string(), "dev".to_string()].into();

    for ask in ["codereview", "dev"] {
        assert!(
            Manager::eligible(
                &account,
                0.90,
                None,
                &pacing,
                false,
                now,
                now_ms,
                false,
                crate::provider::Provider::Anthropic,
                Some(ask),
                Some(ask),
                &reserved,
                &HashSet::new(),
            ),
            "ask={ask}: an explicit ask for the account's own (reserved) group \
             must clear the reserved gate"
        );
        let (gate, _) = Manager::account_gate(
            &account,
            0.90,
            None,
            now,
            now_ms,
            false,
            Some(ask),
            &reserved,
            &HashSet::new(),
        );
        assert_ne!(gate, GateReason::Reserved, "ask={ask}: account_gate agrees");
    }
}
