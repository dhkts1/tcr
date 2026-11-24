//! Group policy: preferences, reserved, parked and control-allowed groups, and hot reload of the group config.

use super::*;

// ---- `--group` prefer-routing (Phase 1: PREFER, never `--only`) -----------

/// The prefer half of the feature: with exactly one account carrying the
/// requested group, an unpinned select with that group MUST land on it —
/// even though the OTHER account would win the ordinary (no-preference)
/// LRU/priority pick. Then, group-out the grouped account (via `disabled`,
/// the simplest hard gate) and confirm the SAME select call REFUSES
/// (`None`) rather than spilling into the pool — an explicit `--group` ask
/// is strict by default (2026-09-13): served by that group or not at all.
///
/// This used to assert a pool fallback here; that was the pre-2026-09-13
/// rule, encoded now by
/// [`group_preference_falls_back_to_the_pool_when_spill_to_pool_is_set`]
/// with the `spillToPool` escape hatch explicitly on.
#[test]
fn group_preference_prefers_then_refuses_when_exhausted() {
    let grouped = account_in_groups("grouped", 5, &["codereview"]);
    let plain = account("plain", 0); // lower priority number sorts FIRST ordinarily
    let manager = manager_of(vec![grouped, plain]);
    let now = OffsetDateTime::now_utc();

    // Sanity control: with NO group requested, priority ordering picks the
    // UNGROUPED account (index 1) — proves the group assertion below is
    // actually exercising the preference, not just priority order.
    assert_eq!(
        manager.select_with_group(&HashSet::new(), now, None, None, "/v1/messages", None, None),
        Some(1),
        "no group requested: ordinary priority order picks the ungrouped account"
    );

    // The whole feature: `--group codereview` prefers index 0 despite its
    // worse priority.
    assert_eq!(
        manager.select_with_group(
            &HashSet::new(),
            now,
            None,
            None,
            "/v1/messages",
            None,
            Some("codereview"),
        ),
        Some(0),
        "a group with capacity must be preferred over a better-priority ungrouped account"
    );

    // Gate the grouped account out entirely (disabled = hard-ineligible).
    {
        let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
        accounts[0].disabled = true;
    }

    // POSITIVE CONTROL: the pool itself is still perfectly servable. Without
    // this, the `None` below would also be satisfied by a fleet where
    // nothing at all could be picked — proving the assertion about
    // strictness rather than about an empty pool.
    assert_eq!(
        manager.select_with_group(&HashSet::new(), now, None, None, "/v1/messages", None, None),
        Some(1),
        "control: the ungrouped pool is servable, so a None below is strictness, not exhaustion"
    );

    // Strict-by-default: the group has no capacity, so an explicit ask for
    // it refuses rather than spilling into the whole pool.
    assert_eq!(
        manager.select_with_group(
            &HashSet::new(),
            now,
            None,
            None,
            "/v1/messages",
            None,
            Some("codereview"),
        ),
        None,
        "group exhausted, no spillToPool set: must refuse, not spill into the pool"
    );
}

/// The escape hatch: with `groupSettings.<g>.spillToPool` set, the SAME
/// exhausted-group scenario as
/// [`group_preference_prefers_then_refuses_when_exhausted`] falls back to
/// the whole pool instead of refusing — the pre-2026-09-13 behaviour,
/// restored per group on request.
#[test]
fn group_preference_falls_back_to_the_pool_when_spill_to_pool_is_set() {
    let grouped = account_in_groups("grouped", 5, &["codereview"]);
    let plain = account("plain", 0);
    let mut config = config_with(vec![grouped, plain]);
    config.group_settings.insert(
        "codereview".to_string(),
        config::GroupSettings {
            reserved: false,
            parked: false,
            allow_control_account: false,
            spill_to_pool: true,
            color: None,
            extra: serde_json::Map::new(),
        },
    );
    let manager = build_manager(config, counting_refresher());
    let now = OffsetDateTime::now_utc();

    // Confirm the preference still applies before exhausting the group —
    // `spillToPool` only changes what happens on exhaustion.
    assert_eq!(
        manager.select_with_group(
            &HashSet::new(),
            now,
            None,
            None,
            "/v1/messages",
            None,
            Some("codereview"),
        ),
        Some(0),
        "spillToPool does not change the prefer half of the feature"
    );

    {
        let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
        accounts[0].disabled = true;
    }

    assert_eq!(
        manager.select_with_group(
            &HashSet::new(),
            now,
            None,
            None,
            "/v1/messages",
            None,
            Some("codereview"),
        ),
        Some(1),
        "spillToPool restores the pre-2026-09-13 rule: group exhausted, fall back to the pool"
    );
}

// ---- reserved groups (a reservation is not a preference) ------------------

/// Like [`config_with`] but with the named groups marked `reserved` in
/// `groupSettings` — mirrors [`config_with_lock`]/[`config_with_control`].
fn config_with_reserved(accounts: Vec<Account>, reserved: &[&str]) -> Config {
    let mut config = config_with(accounts);
    for group in reserved {
        config.group_settings.insert(
            (*group).to_string(),
            config::GroupSettings {
                reserved: true,
                parked: false,
                allow_control_account: false,
                spill_to_pool: false,
                color: None,
                extra: serde_json::Map::new(),
            },
        );
    }
    config
}

/// Semantics test #1 (both directions): unrequested traffic cannot select
/// an account in a reserved group, even when its priority would otherwise
/// win — and the SAME account is reachable by unrequested traffic again
/// once its group is not reserved.
#[test]
fn reserved_group_blocks_unrequested_traffic_both_directions() {
    let reserved_acct = account_in_groups("reserved-acct", 0, &["codereview"]);
    let plain = account("plain", 5); // worse priority — must lose ordinarily
    let manager = build_manager(
        config_with_reserved(vec![reserved_acct, plain], &["codereview"]),
        counting_refresher(),
    );
    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
        Some(1),
        "reserved account excluded from unrequested traffic despite better priority"
    );

    // Direction 2: same accounts, group NOT reserved — priority wins again.
    let unreserved = manager_of(vec![
        account_in_groups("reserved-acct", 0, &["codereview"]),
        account("plain", 5),
    ]);
    assert_eq!(
        unreserved.select(&HashSet::new(), now, None, None, "/v1/messages", None),
        Some(0),
        "same account, group unreserved: unrequested traffic can select it again"
    );
}

/// Semantics test #2: `--group codereview` can still select a reserved
/// `codereview` account — reservation narrows UNREQUESTED traffic, never
/// the group's own members.
#[test]
fn reserved_group_still_selectable_by_its_own_group_ask() {
    let reserved_acct = account_in_groups("reserved-acct", 0, &["codereview"]);
    let plain = account("plain", 5);
    let manager = build_manager(
        config_with_reserved(vec![reserved_acct, plain], &["codereview"]),
        counting_refresher(),
    );
    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager.select_with_group(
            &HashSet::new(),
            now,
            None,
            None,
            "/v1/messages",
            None,
            Some("codereview"),
        ),
        Some(0),
        "--group codereview still selects its own reserved account"
    );
}

/// Semantics test #2b: a RESERVED group stays strict — when its members
/// are all gated, the ask returns `None` instead of spilling into the
/// pool, and unlike an unreserved group this cannot be reopened with
/// `spillToPool` (see [`GroupSettings::spill_to_pool`]'s doc-comment).
///
/// Strict-by-default (2026-09-13) now covers every group, reserved or
/// not — see
/// [`group_preference_prefers_then_refuses_when_exhausted`] for the
/// unreserved case, which behaves identically here. What this test still
/// proves that the unreserved one does not: a reserved group's strictness
/// is NOT an opt-out surface. Without it, a reserved group's request could
/// silently land on an account the operator deliberately walled off from
/// it — measured on the live fleet 2026-09-01, 33 times in one day, every
/// one `reason="all-members-unavailable"`, after pool traffic had
/// rate-limited the group's only member.
#[test]
fn a_reserved_group_never_falls_back_to_the_pool() {
    let reserved_acct = account_in_groups("reserved-acct", 5, &["codereview"]);
    let plain = account("plain", 0); // better priority — would win any pool pick
    let manager = build_manager(
        config_with_reserved(vec![reserved_acct, plain], &["codereview"]),
        counting_refresher(),
    );
    let now = OffsetDateTime::now_utc();

    // Gate the group's only member out entirely (disabled = hard-ineligible),
    // exactly as `group_preference_prefers_then_refuses_when_exhausted` does.
    {
        let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
        accounts[0].disabled = true;
    }

    // POSITIVE CONTROL: the pool itself is still perfectly servable. Without
    // this, the `None` below would also be satisfied by a fleet where nothing
    // at all could be picked — proving the assertion about strictness rather
    // than about an empty pool.
    assert_eq!(
        manager.select_with_group(&HashSet::new(), now, None, None, "/v1/messages", None, None),
        Some(1),
        "control: the ungrouped pool is servable, so a None below is strictness, not exhaustion"
    );

    assert_eq!(
        manager.select_with_group(
            &HashSet::new(),
            now,
            None,
            None,
            "/v1/messages",
            None,
            Some("codereview"),
        ),
        None,
        "a reserved group must never spill into the pool — refuse and let the \
         caller's exhaustion ladder soft-wait or answer an honest 429"
    );
}

/// The escape hatch does not reopen a RESERVED group: `spillToPool: true`
/// on a reserved group's `groupSettings` entry is ignored, because
/// spilling its own traffic into the pool would hand it to an account
/// other traffic cannot reach any other way — the opposite of what
/// `reserved` promises inbound. Same scenario as
/// [`a_reserved_group_never_falls_back_to_the_pool`], with `spillToPool`
/// deliberately set to prove it has no effect here.
#[test]
fn a_reserved_group_ignores_spill_to_pool() {
    let reserved_acct = account_in_groups("reserved-acct", 5, &["codereview"]);
    let plain = account("plain", 0);
    let mut config = config_with(vec![reserved_acct, plain]);
    config.group_settings.insert(
        "codereview".to_string(),
        config::GroupSettings {
            reserved: true,
            parked: false,
            allow_control_account: false,
            spill_to_pool: true,
            color: None,
            extra: serde_json::Map::new(),
        },
    );
    let manager = build_manager(config, counting_refresher());
    let now = OffsetDateTime::now_utc();

    {
        let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
        accounts[0].disabled = true;
    }

    assert_eq!(
        manager.select_with_group(&HashSet::new(), now, None, None, "/v1/messages", None, None),
        Some(1),
        "control: the ungrouped pool is servable"
    );

    assert_eq!(
        manager.select_with_group(
            &HashSet::new(),
            now,
            None,
            None,
            "/v1/messages",
            None,
            Some("codereview"),
        ),
        None,
        "reserved + spillToPool: still refuses — spillToPool cannot reopen a reserved group"
    );
}

/// A strict group's retry hint is sized by when one of ITS members frees,
/// not by whichever account in the fleet un-gates first.
///
/// The fleet-wide [`Manager::retry_after_hint`] minimises over every
/// account, so an unrelated account recovering sooner wins. For a strict
/// `--group` request that number is a promise about capacity the request is
/// not allowed to use: the caller spends its one-shot soft-wait, wakes to
/// find its own member still held, and answers a 429 that was avoidable.
/// The member here is held 10x longer than the non-member precisely so the
/// two hints cannot coincide by accident.
#[test]
fn a_strict_groups_retry_hint_is_sized_by_its_own_member() {
    let member = account_in_groups("member", 0, &["codereview"]);
    let outsider = account("outsider", 1);
    let manager = build_manager(
        config_with_reserved(vec![member, outsider], &["codereview"]),
        counting_refresher(),
    );
    manager.mark_rate_limited(0, 600); // the group's member
    manager.mark_rate_limited(1, 60); // an unrelated account, free much sooner
    // AFTER the marks: each `mark_rate_limited` reads its own clock, so a
    // `now` taken before them is a few microseconds earlier than the
    // holds' anchor and the 60 s hint rounds up to 61 — which failed the
    // `<= 60` control on CI (#189) with nothing wrong in the code.
    let now = OffsetDateTime::now_utc();

    let fleet = manager.retry_after_hint(now, false, crate::provider::Provider::Anthropic);
    let scoped = manager.retry_after_hint_for_group(
        now,
        false,
        crate::provider::Provider::Anthropic,
        "codereview",
    );

    // Control: the fleet-wide hint really is won by the outsider, so the
    // assertion below is measuring the group scoping and not a tautology.
    assert!(
        fleet <= 60,
        "control: fleet-wide hint should follow the sooner outsider, got {fleet}s"
    );
    assert!(
        scoped > 500,
        "a strict group's hint must follow its own held member (~600s), got {scoped}s"
    );
}

/// Semantics test #3: an account in reserved `codereview` + plain `dev` is
/// reachable by BOTH `--group` values and NOT by unrequested traffic.
#[test]
fn reserved_plus_plain_group_reachable_by_either_ask_not_unrequested() {
    let multi = account_in_groups("multi", 0, &["codereview", "dev"]);
    let plain = account("plain", 5);
    let manager = build_manager(
        config_with_reserved(vec![multi, plain], &["codereview"]),
        counting_refresher(),
    );
    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager.select_with_group(
            &HashSet::new(),
            now,
            None,
            None,
            "/v1/messages",
            None,
            Some("codereview"),
        ),
        Some(0),
        "reachable via its reserved group"
    );
    assert_eq!(
        manager.select_with_group(
            &HashSet::new(),
            now,
            None,
            None,
            "/v1/messages",
            None,
            Some("dev"),
        ),
        Some(0),
        "reachable via its plain group too"
    );
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
        Some(1),
        "unrequested traffic still excluded — one reserved group is enough"
    );
}

// ---- parked groups (a whole group out of rotation) ------------------------

/// Like [`config_with_reserved`] but sets `parked` instead — the two flags
/// are independent, so nothing here touches `reserved`.
fn config_with_parked(accounts: Vec<Account>, parked: &[&str]) -> Config {
    let mut config = config_with(accounts);
    for group in parked {
        config.group_settings.insert(
            (*group).to_string(),
            config::GroupSettings {
                reserved: false,
                parked: true,
                allow_control_account: false,
                spill_to_pool: false,
                color: None,
                extra: serde_json::Map::new(),
            },
        );
    }
    config
}

/// THE HARD GATE. A member of a parked group is never selected — not by
/// unrequested traffic, and not by an explicit `--group` ask for the parked
/// group itself, which is where parking differs from reservation: an
/// explicit ask lifts a reservation and does not lift a park.
///
/// The last assertion is the one that makes the ask honest rather than
/// silently rerouted: with the ONLY member of the asked-for group parked,
/// the request refuses (`None`) instead of spilling onto an unrelated
/// account, exactly as a reserved group's own ask does when no member can
/// serve.
#[test]
fn parked_group_member_is_never_selected() {
    let parked_acct = account_in_groups("parked-acct", 0, &["codereview"]);
    let plain = account("plain", 5); // worse priority — must lose ordinarily
    let manager = build_manager(
        config_with_parked(vec![parked_acct, plain], &["codereview"]),
        counting_refresher(),
    );
    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
        Some(1),
        "a parked account must be excluded from unrequested traffic despite better priority"
    );
    assert_eq!(
        manager.select_with_group(
            &HashSet::new(),
            now,
            None,
            None,
            "/v1/messages",
            None,
            Some("codereview"),
        ),
        None,
        "a --group ask for a parked group must not select the parked member, and \
         strict-by-default means it refuses rather than falling back to the pool"
    );

    // Same fixture with the parked group as the fleet's ONLY group member
    // pool: the ask has nowhere honest to go, so it refuses.
    let only_parked = build_manager(
        config_with_parked(
            vec![account_in_groups("parked-acct", 0, &["codereview"])],
            &["codereview"],
        ),
        counting_refresher(),
    );
    assert_eq!(
        only_parked.select_with_group(
            &HashSet::new(),
            now,
            None,
            None,
            "/v1/messages",
            None,
            Some("codereview"),
        ),
        None,
        "with every member parked the ask must refuse, not serve from somewhere else"
    );
}

/// A pin does not survive its group being parked: the pin is RE-KEYED, not
/// merely diverted for one request — the same treatment a newly-reserved
/// group's pin gets, because `parked_blocks` sits inside `account_hard_ok`.
#[test]
fn pin_reclaim_when_the_pinned_accounts_group_is_parked() {
    let manager = build_manager(
        config_with_parked(codereview_acct_and_other(), &["codereview"]),
        counting_refresher(),
    );
    let key = 4242u64;
    let served = select_with_a_fresh_pin_on_the_first_account(&manager, key)
        .expect("the unparked account is still eligible");
    assert_eq!(served, 1, "a parked account must not keep serving its pin");
    let pinned_now = pin_of(&manager, key);
    assert_eq!(
        pinned_now,
        Some(1),
        "the pin itself must move off the parked account"
    );
}

/// The LIVE half: nothing is parked at boot, a config write parks the
/// group, and the very next request — with no restart — sees the gate. This
/// is the claim `tcr group park`'s own help makes to the operator, so it is
/// measured rather than assumed.
#[test]
fn reload_picks_up_a_group_being_parked() {
    let (path, manager) = acct_and_other_on_disk("reload-group-park");

    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
        Some(0),
        "at boot nothing is parked, so the better-priority account serves"
    );

    std::thread::sleep(std::time::Duration::from_millis(20));
    let parked_config = config_with_parked(codereview_acct_and_other(), &["codereview"]);
    config::save(&path, &parked_config).expect("write parked reload config");

    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
        Some(1),
        "a group parked on disk must take effect on the next request, with no restart"
    );
    let snapshot = manager.snapshot(now);
    assert_eq!(
        snapshot.accounts[0].gate,
        GateReason::Parked,
        "and the fleet view names the gate"
    );
    assert_eq!(
        snapshot.accounts[0].parked_groups,
        vec!["codereview".to_string()],
        "naming WHICH group parked the row"
    );

    std::fs::remove_file(&path).ok();
}

/// The two flags are independent and the row's own `disabled` wins the
/// REASON: a hand-disabled member of a parked group reads `disabled`, so
/// the panel never tells an operator a group parked a row they benched
/// themselves.
#[test]
fn a_disabled_member_of_a_parked_group_still_reads_disabled() {
    let mut disabled_member = account_in_groups("benched", 0, &["codereview"]);
    disabled_member.disabled = Some(true);
    let manager = build_manager(
        config_with_parked(
            vec![
                disabled_member,
                account_in_groups("live", 1, &["codereview"]),
                account("other", 5),
            ],
            &["codereview"],
        ),
        counting_refresher(),
    );
    let snapshot = manager.snapshot(OffsetDateTime::now_utc());
    assert_eq!(
        snapshot.accounts[0].gate,
        GateReason::Disabled,
        "the row's own choice outranks its group's"
    );
    assert_eq!(
        snapshot.accounts[1].gate,
        GateReason::Parked,
        "its sibling, benched only by the group, says so"
    );
    assert_eq!(
        snapshot.accounts[0].parked_groups,
        vec!["codereview".to_string()],
        "the parked group is still REPORTED on the disabled row — only the gate differs"
    );
}

// ---- control-account opt-in groups (`allowControlAccount`) ----------------

/// Like [`config_with_reserved`] but sets `allowControlAccount` instead —
/// combined with [`config_with_control`]'s `control_account` set so the
/// fixture is actually control-only, not just labelled.
fn config_with_control_allowed(accounts: Vec<Account>, control: &str, groups: &[&str]) -> Config {
    let mut config = config_with_control(accounts, control);
    for group in groups {
        config.group_settings.insert(
            (*group).to_string(),
            config::GroupSettings {
                reserved: false,
                parked: false,
                allow_control_account: true,
                spill_to_pool: false,
                color: None,
                extra: serde_json::Map::new(),
            },
        );
    }
    config
}

/// The whole feature, both directions: a group whose only member is the
/// control account cannot serve `--group research` inference by default —
/// strict-by-default (2026-09-13) means that ask now refuses rather than
/// spilling to the pool, since an explicit `--group` ask is served by that
/// group or not at all — but the SAME fixture, with `allowControlAccount`
/// set on `research`, serves it.
#[test]
fn control_only_group_serves_inference_only_once_opted_in() {
    let control = account_in_groups("control-acct", 0, &["research"]);
    let other = account("other", 5);

    let blocked = build_manager(
        config_with_control(vec![control.clone(), other.clone()], "control-acct"),
        counting_refresher(),
    );
    let now = OffsetDateTime::now_utc();
    assert_eq!(
        blocked.select_with_group(&HashSet::new(), now, None, None, "/v1/messages", None, None),
        Some(1),
        "control: with no group requested, the pool is servable"
    );
    assert_eq!(
        blocked.select_with_group(
            &HashSet::new(),
            now,
            None,
            None,
            "/v1/messages",
            None,
            Some("research"),
        ),
        None,
        "not opted in: strict by default, refuses rather than falling back to the pool"
    );

    let mut opted_in_config = config_with_control(vec![control, other], "control-acct");
    opted_in_config.group_settings.insert(
        "research".to_string(),
        config::GroupSettings {
            reserved: false,
            parked: false,
            allow_control_account: true,
            spill_to_pool: false,
            color: None,
            extra: serde_json::Map::new(),
        },
    );
    let opted_in = build_manager(opted_in_config, counting_refresher());
    assert_eq!(
        opted_in.select_with_group(
            &HashSet::new(),
            now,
            None,
            None,
            "/v1/messages",
            None,
            Some("research"),
        ),
        Some(0),
        "opted in: the group's own `--group` ask may now select the control account"
    );
}

/// The opt-in is scoped to the GROUP, not to the control account globally:
/// an unrequested (no `--group`) inference request must still never select
/// the control account, even when its only group has opted in.
#[test]
fn control_opt_in_does_not_affect_unrequested_traffic() {
    let control = account_in_groups("control-acct", 0, &["research"]);
    let other = account("other", 5);
    let manager = build_manager(
        config_with_control_allowed(vec![control, other], "control-acct", &["research"]),
        counting_refresher(),
    );
    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
        Some(1),
        "unrequested traffic must still never select the control account"
    );
}

/// Semantics test #4 — THE test that proves the feature is real: a
/// session already pinned to an account whose group becomes reserved
/// (the on-disk-pin-predates-the-reservation case the bridge names) is
/// re-pinned away on its next UNREQUESTED request, not left serving the
/// reserved account forever. Seeds the pin directly into `manager.affinity`
/// — the shape a pin restored from disk at boot takes, exactly the
/// scenario the bridge's "reachable by a proxy restart or by a pin that
/// predates the reservation" sentence describes.
#[test]
fn pin_reclaim_when_the_pinned_accounts_group_is_reserved() {
    let manager = build_manager(
        config_with_reserved(codereview_acct_and_other(), &["codereview"]),
        counting_refresher(),
    );
    let key = 4242u64;
    let served = select_with_a_fresh_pin_on_the_first_account(&manager, key)
        .expect("the plain account is still eligible");
    assert_eq!(
        served, 1,
        "a reserved account must not keep serving an unrequested pinned session"
    );

    // The pin itself must have MOVED — "reservation is not a preference":
    // an ordinary per-request divert would leave the OLD index pinned and
    // only serve elsewhere for this one request; that is NOT enough here.
    let pinned_now = pin_of(&manager, key);
    assert_eq!(
        pinned_now,
        Some(1),
        "the pin itself must move off the reserved account, not merely divert one request"
    );
}

/// Pin session `key` to account 0 as of now, then select for it with nothing tried.
fn select_with_a_fresh_pin_on_the_first_account(manager: &Manager, key: u64) -> Option<usize> {
    let now = OffsetDateTime::now_utc();
    seed_pin(manager, key, 0, odt_to_ms(now));
    manager.select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
}

// ---- live group reload (problem 1: group edits appear to do nothing) ------

/// `acct` at priority 0 in group `codereview`, then `other` at priority 5 in no group.
fn codereview_acct_and_other() -> Vec<Account> {
    vec![
        account_in_groups("acct", 0, &["codereview"]),
        account("other", 5),
    ]
}

/// A manager booted from a config file tagged `tag` that holds
/// [`codereview_acct_and_other`] and nothing parked or reserved, and that file's path.
fn acct_and_other_on_disk(tag: &str) -> (PathBuf, Arc<Manager>) {
    let path = tmp_config_path(tag);
    let boot = config_with(codereview_acct_and_other());
    config::save(&path, &boot).expect("write initial reload config");
    let manager = build_manager_with_path(boot, path.clone());
    (path, manager)
}

/// A `Config` with one account per `(name, priority, groups)` tuple —
/// `groups = &[]` means no `groups` key, matching [`account`]/
/// [`account_in_groups`]'s own split. Shared setup for every reload test
/// below: called once to build the manager's BOOT config, and called
/// again (saved straight to `path`, never through the manager) to
/// simulate an out-of-band edit — a hand edit, or a sibling `tcr group`
/// invocation — while the manager keeps running.
fn reload_config(accounts: &[(&str, i64, &[&str])]) -> Config {
    let built: Vec<Account> = accounts
        .iter()
        .map(|&(name, priority, groups)| {
            if groups.is_empty() {
                account(name, priority)
            } else {
                account_in_groups(name, priority, groups)
            }
        })
        .collect();
    config_with(built)
}

/// THE test for problem 1: editing a temp config's group membership and
/// touching its mtime makes a SUBSEQUENT snapshot report the new groups —
/// without rebuilding the `Manager`. If only one test in this section
/// survives, this is it.
#[test]
fn snapshot_picks_up_a_group_edit_without_rebuilding_the_manager() {
    let path = tmp_config_path("reload-feature");
    let boot = reload_config(&[("acct", 0, &["dev"])]);
    config::save(&path, &boot).expect("write initial reload config");
    let manager = build_manager_with_path(boot, path.clone());

    let before = manager.snapshot(OffsetDateTime::now_utc());
    assert_eq!(before.accounts[0].groups, vec!["dev".to_string()]);

    // A fast filesystem can produce two writes with the SAME mtime at
    // whole-second resolution; sleep past that so the edit is actually
    // observable via mtime, the same concern any mtime-triggered reload
    // design has.
    std::thread::sleep(std::time::Duration::from_millis(20));
    let edited = reload_config(&[("acct", 0, &["ops", "burst"])]);
    config::save(&path, &edited).expect("write edited reload config");

    let after = manager.snapshot(OffsetDateTime::now_utc());
    let mut groups = after.accounts[0].groups.clone();
    groups.sort();
    assert_eq!(
        groups,
        vec!["burst".to_string(), "ops".to_string()],
        "a snapshot after the file changed must report the NEW groups, without \
         rebuilding the Manager"
    );

    std::fs::remove_file(&path).ok();
}

/// A reload does NOT disturb accounts, credentials or tokens — only
/// `groups`/`groupSettings` move. Rewrites the file with the SAME account
/// identity (name match), a DIFFERENT access token, AND a group change in
/// one write, then asserts the in-memory access token is untouched while
/// the group DID move — proving the scope, not just the happy path.
#[test]
fn reload_never_touches_accounts_credentials_or_tokens() {
    let path = tmp_config_path("reload-scope");
    let boot = reload_config(&[("acct", 0, &["dev"])]);
    config::save(&path, &boot).expect("write initial reload config");
    let manager = build_manager_with_path(boot, path.clone());

    let original_token = {
        let accounts = manager.accounts.read().expect("accounts lock poisoned");
        accounts[0].access_token.clone()
    };

    std::thread::sleep(std::time::Duration::from_millis(20));
    let mut tampered = reload_config(&[("acct", 0, &["ops"])]);
    tampered.accounts[0].access_token = "at-DIFFERENT-token-must-not-apply".to_string();
    config::save(&path, &tampered).expect("write tampered reload config");

    manager.reload_groups_if_changed();

    let accounts = manager.accounts.read().expect("accounts lock poisoned");
    assert_eq!(
        accounts[0].access_token, original_token,
        "reload must never touch the access token — only group fields"
    );
    assert_eq!(
        accounts[0].groups,
        vec!["ops".to_string()],
        "the group change in the SAME file write must still apply"
    );
    drop(accounts);
    std::fs::remove_file(&path).ok();
}

/// A malformed config at reload time keeps the previous in-memory groups
/// (never blanks the fleet) and does not advance the remembered mtime, so
/// a subsequent FIX self-heals without the file needing to change again
/// after that.
#[test]
fn reload_keeps_current_groups_on_a_malformed_config() {
    let path = tmp_config_path("reload-malformed");
    let boot = reload_config(&[("acct", 0, &["dev"])]);
    config::save(&path, &boot).expect("write initial reload config");
    let manager = build_manager_with_path(boot, path.clone());

    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(&path, b"{ this is not valid json").expect("write malformed config");
    manager.reload_groups_if_changed();

    let accounts = manager.accounts.read().expect("accounts lock poisoned");
    assert_eq!(
        accounts[0].groups,
        vec!["dev".to_string()],
        "a malformed config at reload time must keep the previous groups, not blank them"
    );
    drop(accounts);

    // Self-heal: fixing the file (mtime necessarily advances again, since
    // this write follows the malformed one) makes the NEXT reload apply.
    std::thread::sleep(std::time::Duration::from_millis(20));
    let fixed = reload_config(&[("acct", 0, &["fixed"])]);
    config::save(&path, &fixed).expect("write fixed reload config");
    manager.reload_groups_if_changed();
    let accounts = manager.accounts.read().expect("accounts lock poisoned");
    assert_eq!(accounts[0].groups, vec!["fixed".to_string()]);
    drop(accounts);
    std::fs::remove_file(&path).ok();
}

/// APP-35: an account APPENDED to the file after boot must be admitted by
/// the same reload path that already picks up a group edit — not just
/// counted, but genuinely selectable, which the disabled-control below
/// proves. Never touches or removes the two accounts that were already
/// there (`reload_never_touches_accounts_credentials_or_tokens` already
/// pins that half; this is additive).
#[test]
fn reload_admits_an_account_added_to_the_file_after_boot() {
    let path = tmp_config_path("reload-hot-add");
    let boot = reload_config(&[("acct-a", 0, &[]), ("acct-b", 5, &[])]);
    config::save(&path, &boot).expect("write initial reload config");
    let manager = build_manager_with_path(boot, path.clone());
    assert_eq!(
        manager
            .accounts
            .read()
            .expect("accounts lock poisoned")
            .len(),
        2,
        "control: boots with exactly the two accounts on disk"
    );

    // The operator appends a third account to the file by hand while the
    // proxy runs.
    std::thread::sleep(std::time::Duration::from_millis(20));
    let appended = reload_config(&[("acct-a", 0, &[]), ("acct-b", 5, &[]), ("acct-c", 9, &[])]);
    config::save(&path, &appended).expect("write appended reload config");
    manager.reload_groups_if_changed();

    {
        let accounts = manager.accounts.read().expect("accounts lock poisoned");
        assert_eq!(
            accounts.len(),
            3,
            "the account added to the file must appear in the running manager"
        );
    }

    // Gate the two original accounts out so a successful select can only
    // mean the new one is genuinely selectable, not merely present in a
    // list nothing reads.
    {
        let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
        accounts[0].disabled = true;
        accounts[1].disabled = true;
    }
    let now = OffsetDateTime::now_utc();
    let idx = manager.select(&HashSet::new(), now, None, None, "/v1/messages", None);
    let accounts = manager.accounts.read().expect("accounts lock poisoned");
    assert_eq!(
        idx.and_then(|i| accounts.get(i)).map(|a| a.name.as_str()),
        Some("acct-c"),
        "the newly admitted account must be selectable, not just counted: got {idx:?}"
    );
    drop(accounts);
    std::fs::remove_file(&path).ok();
}

/// A Codex row the file names while the proxy runs is ADMITTED by the reload
/// even when the running fleet holds an Anthropic row of the same name with no
/// uuid, and that Anthropic row keeps its own groups. Provider-blind, the name
/// fallback called the Codex row already known through the Anthropic row, so it
/// was never admitted, and the group carry handed the Codex row's groups to the
/// Anthropic one. The Codex row carries a uuid, as a real one does (the ChatGPT
/// account id): it does not help, because the Anthropic side has none.
///
/// Watched red: drop the provider guard from `identity::same_identity` and the
/// Codex row is never admitted, while the Anthropic row takes its groups.
#[test]
fn reload_admits_a_codex_row_named_like_a_running_anthropic_row() {
    let path = tmp_config_path("reload-codex-beside-anthropic");
    let boot = config_with(vec![account_in_groups(
        "alice@example.com",
        0,
        &["anthropic-team"],
    )]);
    config::save(&path, &boot).expect("write initial reload config");
    let manager = build_manager_with_path(boot, path.clone());

    std::thread::sleep(std::time::Duration::from_millis(20));
    let codex = Account {
        provider: crate::provider::Provider::Codex,
        account_uuid: Some("11111111-1111-4111-8111-111111111111".to_string()),
        ..account_in_groups("alice@example.com", 1, &["codex-team"])
    };
    config::save(&path, &config_with(vec![codex])).expect("write reload config");
    manager.reload_groups_if_changed();

    let rows: Vec<(String, crate::provider::Provider, Vec<String>)> = manager
        .accounts
        .read()
        .expect("accounts lock poisoned")
        .iter()
        .map(|a| (a.name.clone(), a.provider, a.groups.clone()))
        .collect();
    assert_eq!(
        rows,
        vec![
            (
                "alice@example.com".to_string(),
                crate::provider::Provider::Anthropic,
                vec!["anthropic-team".to_string()]
            ),
            (
                "alice@example.com".to_string(),
                crate::provider::Provider::Codex,
                vec!["codex-team".to_string()]
            ),
        ],
        "the Codex row is admitted beside the Anthropic row, each with its own groups"
    );
    std::fs::remove_file(&path).ok();
}

/// C-4 regression: `Self::account_egress` indexes `self.config.accounts`
/// with a RUNTIME index (`self.accounts`). Before this fix, the APP-35
/// admission arm above (`reload_admits_an_account_added_to_the_file_after_boot`)
/// pushed a newly discovered account onto the runtime vector ONLY, never
/// onto `self.config.accounts`, so after one hot-add, `self.accounts` has
/// N+1 rows and `self.config.accounts` still has N. `account_egress(N)` then
/// reads `config.accounts.get(N)`, which is out of bounds and `None`, so a
/// STRICT pin on the newly admitted account is silently read as no pin at
/// all.
#[test]
fn reload_keeps_account_egress_index_aligned_with_a_hot_added_strict_pin() {
    let path = tmp_config_path("reload-egress-index");
    let pinned_peer = tcr_peer_wire::PeerId([9u8; 32]);
    let boot = config_with(vec![account("acct-a", 0)]);
    config::save(&path, &boot).expect("write initial reload config");
    let manager = build_manager_with_path(boot, path.clone());
    assert_eq!(
        manager
            .accounts
            .read()
            .expect("accounts lock poisoned")
            .len(),
        1,
        "control: boots with exactly the one account on disk"
    );

    std::thread::sleep(std::time::Duration::from_millis(20));
    let appended = config_with(vec![
        account("acct-a", 0),
        Account {
            egress: config::Egress::Via(pinned_peer),
            egress_strict: true,
            ..account("acct-b", 5)
        },
    ]);
    config::save(&path, &appended).expect("write appended reload config");
    manager.reload_groups_if_changed();

    let idx = {
        let accounts = manager.accounts.read().expect("accounts lock poisoned");
        assert_eq!(
            accounts.len(),
            2,
            "the newly admitted account must appear in the runtime rotation"
        );
        accounts
            .iter()
            .position(|a| a.name == "acct-b")
            .expect("acct-b was admitted by reload")
    };

    let pin = manager.account_egress(idx);
    assert_eq!(
        pin,
        Some(config::EgressPin {
            egress: config::Egress::Via(pinned_peer),
            strict: true,
        }),
        "a strict pin on a hot-admitted account must be readable by its runtime \
         index, not silently None (fail open) because self.config.accounts \
         lagged self.accounts: got {pin:?}"
    );

    std::fs::remove_file(&path).ok();
}

/// A session pinned to an account that becomes reserved VIA RELOAD (not
/// construction-time, unlike
/// [`pin_reclaim_when_the_pinned_accounts_group_is_reserved`]) is
/// re-keyed on its next unrequested `select`.
#[test]
fn reload_re_keys_a_pin_when_its_group_becomes_reserved() {
    let (path, manager) = acct_and_other_on_disk("reload-pin-reclaim");

    let now = OffsetDateTime::now_utc();
    let key = 9191u64;
    seed_pin(&manager, key, 0, odt_to_ms(now));

    // Reserve 'codereview' via a fresh file write — no construction-time
    // reservation at all, unlike the sibling test above.
    std::thread::sleep(std::time::Duration::from_millis(20));
    let reserved_config = config_with_reserved(codereview_acct_and_other(), &["codereview"]);
    config::save(&path, &reserved_config).expect("write reserved reload config");

    let served = manager
        .select(&HashSet::new(), now, None, Some(key), "/v1/messages", None)
        .expect("the plain account is still eligible");
    assert_eq!(
        served, 1,
        "a group reserved via RELOAD must not keep serving an unrequested pinned session"
    );
    let pinned_now = pin_of(&manager, key);
    assert_eq!(
        pinned_now,
        Some(1),
        "the pin must move off the newly-reserved account, not merely divert one request"
    );

    std::fs::remove_file(&path).ok();
}

/// Same live-reload cadence as [`reload_re_keys_a_pin_when_its_group_becomes_reserved`],
/// applied to `allowControlAccount`: a control-only group cannot serve its
/// own `--group` ask at boot, and a config write that opts it in — with NO
/// restart — makes the very same request land on the control account on
/// its next call.
#[test]
fn reload_picks_up_a_group_opting_in_to_the_control_account() {
    let path = tmp_config_path("reload-control-allowed");
    let boot = config_with_control(
        vec![
            account_in_groups("control-acct", 0, &["research"]),
            account("other", 5),
        ],
        "control-acct",
    );
    config::save(&path, &boot).expect("write initial reload config");
    let manager = build_manager_with_path(boot, path.clone());

    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager.select_with_group(
            &HashSet::new(),
            now,
            None,
            None,
            "/v1/messages",
            None,
            Some("research"),
        ),
        None,
        "not opted in at boot: strict by default, refuses rather than falling back to the pool"
    );

    std::thread::sleep(std::time::Duration::from_millis(20));
    let mut opted_in = config_with_control(
        vec![
            account_in_groups("control-acct", 0, &["research"]),
            account("other", 5),
        ],
        "control-acct",
    );
    opted_in.group_settings.insert(
        "research".to_string(),
        config::GroupSettings {
            reserved: false,
            parked: false,
            allow_control_account: true,
            spill_to_pool: false,
            color: None,
            extra: serde_json::Map::new(),
        },
    );
    config::save(&path, &opted_in).expect("write opted-in reload config");

    assert_eq!(
        manager.select_with_group(
            &HashSet::new(),
            now,
            None,
            None,
            "/v1/messages",
            None,
            Some("research"),
        ),
        Some(0),
        "a live opt-in write must be picked up with no restart"
    );

    std::fs::remove_file(&path).ok();
}

/// `group` settings as `(reserved, spill_to_pool)` for the one group `g`.
fn config_with_g_settings(accounts: Vec<Account>, reserved: bool, spill: bool) -> Config {
    let mut config = config_with(accounts);
    config.group_settings.insert(
        "g".to_string(),
        config::GroupSettings {
            reserved,
            spill_to_pool: spill,
            ..Default::default()
        },
    );
    config
}

/// A `--group g` select reads every group setting from ONE version of the config.
///
/// Two versions, both strict for `g`: (reserved false, spill false) and (reserved true,
/// spill true). The only member of `g` is disabled, so a strict ask refuses and a spill
/// ask lands on the outsider `o`. A select that read `reserved` from the first version and
/// `spill` from the second would see (false, true), the one mix that is not strict, and
/// route the request to `o`. A thread flips between the two versions while this one
/// selects; any `Some` is a torn read.
#[test]
fn a_group_reload_mid_select_never_routes_on_a_mix_of_two_versions() {
    let accounts = || {
        vec![
            Account {
                disabled: Some(true),
                ..account_in_groups("m", 0, &["g"])
            },
            account("o", 0),
        ]
    };
    let neither = config_with_g_settings(accounts(), false, false);
    let both = config_with_g_settings(accounts(), true, true);
    let refresher = || counting_refresher();
    let select_g = |manager: &Manager| {
        manager.select_with_group(
            &HashSet::new(),
            OffsetDateTime::now_utc(),
            None,
            None,
            "/v1/messages",
            None,
            Some("g"),
        )
    };

    // Positive control: the mixed version itself does route to `o`.
    let mixed = build_manager(config_with_g_settings(accounts(), false, true), refresher());
    assert_eq!(select_g(&mixed), Some(1), "the torn mix must route to `o`");
    let manager = build_manager(neither.clone(), refresher());
    assert_eq!(select_g(&manager), None, "(false, false) is strict");
    manager.apply_group_reload(&both);
    assert_eq!(select_g(&manager), None, "(true, true) is strict");

    let stop = Arc::new(AtomicBool::new(false));
    let flipper = {
        let manager = Arc::clone(&manager);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                manager.apply_group_reload(&neither);
                manager.apply_group_reload(&both);
            }
        })
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    let mut torn = 0usize;
    let mut selects = 0usize;
    while std::time::Instant::now() < deadline {
        selects += 1;
        if select_g(&manager).is_some() {
            torn += 1;
        }
    }
    stop.store(true, Ordering::Relaxed);
    assert!(flipper.join().is_ok(), "the reload thread panicked");
    assert_eq!(torn, 0, "{torn} of {selects} selects routed on a torn read");
}
