//! `add_or_update_account`: matching a submission to a row, appending, priorities, persisting.

use super::*;

// ---- live account-add identity resolution (`POST /_tcr/accounts`) ----

/// Submit `submission` and assert it replaced the credential on row 0.
fn assert_it_updates_the_first_row(manager: &Manager, submission: Account) {
    let outcome = manager.add_or_update_account(submission);
    std::assert_matches!(outcome, AddAccountOutcome::Updated { idx: 0, .. });
}

/// F1 — CRITICAL regression. `{alice@example.com, uuid 1111…, rt-CORP}` is
/// on record; a submission of `{alice@example.com, uuid 2222…, rt-PERSONAL}`
/// is a DIFFERENT PERSON who happens to share a display name. The old rule
/// (`identity::match_one`, which goes through `Queryable` and cannot compare
/// `account_uuid` at all) matched it onto Corp's row and overwrote its
/// single-use refresh token — unrecoverable without a hand re-auth. It must
/// APPEND instead, leaving Corp's row untouched, in memory and on disk.
#[test]
fn add_or_update_account_never_overwrites_a_different_persons_refresh_token() {
    let corp = Account {
        account_uuid: Some("uuid-1111".to_string()),
        access_token: "at-CORP".to_string(),
        refresh_token: Some("rt-CORP".to_string()),
        ..account("alice@example.com", 0)
    };
    let (manager, path) = build_manager_with_disk(config_with(vec![corp]), "f1-uuid-mismatch");

    let submission = Account {
        account_uuid: Some("uuid-2222".to_string()),
        access_token: "at-PERSONAL".to_string(),
        refresh_token: Some("rt-PERSONAL".to_string()),
        ..account("alice@example.com", 0)
    };
    let outcome = manager.add_or_update_account(submission);

    let (idx, persist) = match outcome {
        AddAccountOutcome::Added { idx, persist, .. } => (idx, persist),
        other => panic!(
            "a different account_uuid under the same name must APPEND, not \
             update or refuse: {other:?}"
        ),
    };
    assert_eq!(idx, 1, "appended at the end, Corp's row never moves");
    assert_eq!(persist, AddPersist::Persisted);

    // In memory: Corp's row is byte-identical.
    {
        let accounts = manager.accounts.read().expect("accounts lock poisoned");
        assert_eq!(accounts.len(), 2);
        assert_eq!(
            accounts[0].refresh_token.as_deref(),
            Some("rt-CORP"),
            "Corp's single-use refresh token was overwritten in memory"
        );
        assert_eq!(accounts[0].access_token, "at-CORP");
        assert_eq!(accounts[1].refresh_token.as_deref(), Some("rt-PERSONAL"));
    }

    // On disk: same.
    let reloaded = config::load(&path).expect("reload persisted config");
    assert_eq!(reloaded.accounts.len(), 2);
    assert_eq!(
        reloaded.accounts[0].refresh_token.as_deref(),
        Some("rt-CORP"),
        "Corp's single-use refresh token was overwritten on disk"
    );
    assert_eq!(
        reloaded.accounts[1].refresh_token.as_deref(),
        Some("rt-PERSONAL")
    );

    std::fs::remove_file(&path).ok();
}

/// F5 follow-up: the re-add target `add_or_update_account`'s `Updated`
/// arm hands to `persist_replaced` must carry the account's EXISTING
/// exit-node pin, never hardcode the unpinned default. The bug is
/// unobservable on the common path (`persist_replaced`'s `Resolved::One`
/// arm reads only `access_token`/`refresh_token`/`expires_at` off the
/// target, never touching `egress` when a matching config/disk row is
/// found, so egress survives untouched either way there). It bites in
/// `persist_replaced`'s APPEND arm, reached when the in-memory
/// `self.accounts` runtime row still exists but the on-disk document has
/// lost its counterpart entry (an operator hand-editing `tcr-peers`-style
/// config, or a partial write): `merge_account`'s `EntryMatch::None` arm
/// then serializes the WHOLE target object as a fresh disk row, `egress`
/// included, so with the hardcoded default, the pin the in-memory config
/// still remembers never reaches the newly recreated disk row.
#[test]
fn add_or_update_account_carries_an_existing_strict_egress_pin_into_a_recreated_disk_row() {
    let pinned_peer = tcr_peer_wire::PeerId([7u8; 32]);
    let pinned = Account {
        account_uuid: Some("uuid-pinned".to_string()),
        access_token: "at-old".to_string(),
        refresh_token: Some("rt-old".to_string()),
        egress: config::Egress::Via(pinned_peer),
        egress_strict: true,
        ..account("pinned@example.com", 0)
    };
    let (manager, path) =
        build_manager_with_disk(config_with(vec![pinned]), "f5-egress-pin-carried");

    // Simulate the disk document losing its entry for this account while
    // `self.config` in memory (the manager never re-reads accounts from
    // disk, only groups reload) still carries the full row, pin
    // included: a raw JSON edit, never through `config::save`, which
    // would go through `self.config` and defeat the scenario.
    let mut doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    doc["accounts"].as_array_mut().unwrap().clear();
    std::fs::write(&path, serde_json::to_string_pretty(&doc).unwrap()).unwrap();

    // Same identity, fresh credentials, and, like a real re-add through
    // `POST /_tcr/accounts`, no opinion of its own about egress: the
    // submission carries only what `account(..)` builds, i.e. Local/false.
    let submission = Account {
        account_uuid: Some("uuid-pinned".to_string()),
        access_token: "at-new".to_string(),
        refresh_token: Some("rt-new".to_string()),
        ..account("pinned@example.com", 0)
    };
    let outcome = manager.add_or_update_account(submission);
    match outcome {
        AddAccountOutcome::Updated { persist, .. } => {
            assert_eq!(persist, AddPersist::Persisted);
        }
        other => panic!("same identity, fresh credentials must UPDATE in place: {other:?}"),
    }

    // DISK: the row `persist_replaced` recreates must carry the pin
    // forward, not reset it to Local.
    let reloaded = config::load(&path).expect("reload persisted config");
    assert_eq!(reloaded.accounts.len(), 1);
    assert_eq!(
        reloaded.accounts[0].egress,
        config::Egress::Via(pinned_peer),
        "the recreated disk row must carry the account's existing egress pin forward, \
         not reset it to Local"
    );
    assert!(
        reloaded.accounts[0].egress_strict,
        "the recreated disk row must carry the account's existing strict flag forward"
    );

    std::fs::remove_file(&path).ok();
}

/// F2 — CRITICAL regression. A legacy row with NO stored org, met by a
/// submission carrying the row's OWN org (even just profiled for the first
/// time), must resolve to the SAME row on both the live and durable halves
/// — never split into a second live row backed by the same one disk entry.
#[test]
fn add_or_update_account_backfills_a_legacy_no_org_row_instead_of_splitting_the_fleet() {
    let legacy = Account {
        account_uuid: Some("uuid-legacy".to_string()),
        refresh_token: Some("rt-legacy".to_string()),
        ..account("alice@example.com", 0)
    };
    let (manager, path) = build_manager_with_disk(config_with(vec![legacy]), "f2-legacy-org");

    let submission = Account {
        account_uuid: Some("uuid-legacy".to_string()),
        org_uuid: Some("org-corp-uuid".to_string()),
        org_name: Some("Corp".to_string()),
        access_token: "at-fresh".to_string(),
        refresh_token: Some("rt-fresh".to_string()),
        ..account("alice@example.com", 0)
    };
    let outcome = manager.add_or_update_account(submission);

    let (idx, persist) = match outcome {
        AddAccountOutcome::Updated { idx, persist, .. } => (idx, persist),
        other => panic!(
            "a legacy no-org row meeting its own org must UPDATE in place, \
             not split the fleet: {other:?}"
        ),
    };
    assert_eq!(idx, 0);
    assert_eq!(persist, AddPersist::Persisted);

    // LIVE: one row, now carrying the org and the fresh credentials.
    {
        let accounts = manager.accounts.read().expect("accounts lock poisoned");
        assert_eq!(
            accounts.len(),
            1,
            "a second live row appeared — the split brain this fixes"
        );
        assert_eq!(accounts[0].org_uuid.as_deref(), Some("org-corp-uuid"));
        assert_eq!(accounts[0].org_name.as_deref(), Some("Corp"));
        assert_eq!(accounts[0].refresh_token.as_deref(), Some("rt-fresh"));
    }

    // DISK: agrees — one entry, same identity, now carrying the org too, so
    // the NEXT submission (a genuinely different org) requires an exact
    // match instead of tolerating a still-unknown org key forever.
    let reloaded = config::load(&path).expect("reload persisted config");
    assert_eq!(reloaded.accounts.len(), 1, "disk split into a second entry");
    assert_eq!(
        reloaded.accounts[0].org_uuid.as_deref(),
        Some("org-corp-uuid")
    );
    assert_eq!(
        reloaded.accounts[0].refresh_token.as_deref(),
        Some("rt-fresh")
    );

    std::fs::remove_file(&path).ok();
}

/// F3 — HIGH regression. The old two-lock version resolved under a READ
/// lock, released it, then took a separate WRITE lock to append — so N
/// concurrent submissions of the same brand-new identity could all resolve
/// "no match" before any of them had appended (measured 73 duplicate rows
/// in 200 concurrent rounds). Fire N concurrent adds of ONE new identity and
/// assert exactly one caller appended, on both halves.
#[test]
fn concurrent_adds_of_one_new_identity_never_duplicate_the_live_row() {
    use std::thread;
    let (manager, path) = build_manager_with_disk(
        config_with(vec![account("alice@example.com", 0)]),
        "f3-toctou",
    );

    let n = 16usize;
    let submission = Account {
        account_uuid: Some("uuid-new".to_string()),
        access_token: "at-new".to_string(),
        refresh_token: Some("rt-new".to_string()),
        ..account("carol@example.com", 0)
    };

    let handles: Vec<_> = (0..n)
        .map(|_| {
            let m = Arc::clone(&manager);
            let acct = submission.clone();
            thread::spawn(move || m.add_or_update_account(acct))
        })
        .collect();
    let outcomes: Vec<AddAccountOutcome> = handles
        .into_iter()
        .map(|h| h.join().expect("add_or_update_account thread panicked"))
        .collect();

    let added = outcomes
        .iter()
        .filter(|o| matches!(o, AddAccountOutcome::Added { .. }))
        .count();
    let updated = outcomes
        .iter()
        .filter(|o| matches!(o, AddAccountOutcome::Updated { .. }))
        .count();
    assert_eq!(
        added, 1,
        "exactly one concurrent caller may append a brand-new identity; \
         the rest must see it as already-added (Updated): added={added} updated={updated}"
    );
    assert_eq!(updated, n - 1);

    let accounts = manager.accounts.read().expect("accounts lock poisoned");
    assert_eq!(
        accounts.len(),
        2,
        "alice's row plus exactly one new row for carol — not a duplicate"
    );
    drop(accounts);

    let reloaded = config::load(&path).expect("reload persisted config");
    assert_eq!(
        reloaded.accounts.len(),
        2,
        "disk duplicated the new identity"
    );

    std::fs::remove_file(&path).ok();
}

/// A follow-up regression on the Added path (distinct from the F3 TOCTOU
/// finding above, and from `config::save_account`'s identical fix on the
/// durable side): appending an account with no explicit priority must
/// join the BACK of the LIVE fleet — `max(existing priorities) + 1` — not
/// the 0 that `AccountRuntime::from_config`'s `unwrap_or(0)` reads from an
/// absent `priority`, which would silently promote a freshly added
/// account to the PRIMARY tier ahead of the established fleet.
#[test]
fn add_or_update_account_added_path_assigns_max_plus_one_priority_when_none_submitted() {
    let (manager, path) = build_manager_with_disk(
        config_with(vec![
            account("alice@example.com", 0),
            account("bob@example.com", 1),
        ]),
        "live-default-priority",
    );

    let submission = Account {
        priority: None,
        ..account("carol@example.com", 0)
    };
    let idx = match manager.add_or_update_account(submission) {
        AddAccountOutcome::Added { idx, .. } => idx,
        other => panic!("expected Added: {other:?}"),
    };

    {
        let accounts = manager.accounts.read().expect("accounts lock poisoned");
        assert_eq!(
            accounts[idx].priority, 2,
            "an added account with no explicit priority must join the back \
             of the live fleet, not the default-derived 0"
        );
    }

    let reloaded = config::load(&path).expect("reload persisted config");
    assert_eq!(
        reloaded.accounts[idx].priority,
        Some(2),
        "the durable half must agree with the live rotation on the assigned priority"
    );

    std::fs::remove_file(&path).ok();
}

/// An explicit priority submitted on the Added path is never overridden by
/// the max+1 default — mirrors the durable-side test of the same name in
/// `config.rs`.
#[test]
fn add_or_update_account_added_path_keeps_an_explicit_priority() {
    let (manager, path) = build_manager_with_disk(
        config_with(vec![account("alice@example.com", 0)]),
        "live-explicit-priority",
    );

    let submission = Account {
        priority: Some(99),
        ..account("carol@example.com", 0)
    };
    let idx = match manager.add_or_update_account(submission) {
        AddAccountOutcome::Added { idx, .. } => idx,
        other => panic!("expected Added: {other:?}"),
    };

    let accounts = manager.accounts.read().expect("accounts lock poisoned");
    assert_eq!(accounts[idx].priority, 99);
    drop(accounts);

    std::fs::remove_file(&path).ok();
}

/// Before the first fix, the Added path's `AccountRuntime::from_config(&account)`
/// ran INSIDE the `self.accounts.write()` block — meaning
/// `build_serving_client`'s `.expect("build reqwest client")` executed while
/// the write guard was held. A panic there poisons the `RwLock` for every
/// other caller in this module: `http_client`, `select`, `snapshot`,
/// `record_stream_error` and ~93 more all `.expect("accounts lock
/// poisoned")`, so one recoverable client-build failure would take down the
/// whole proxy.
///
/// A second round moved the build OUTSIDE `self.accounts`'s lock but left
/// it inside `config_write` (held from function entry in that version) —
/// so the SAME panic instead poisoned `config_write`, which is arguably
/// worse: `persist_tokens` and `persist_now`'s shutdown flush both
/// `.expect()` it, so a poisoned `config_write` stops rotated refresh
/// tokens from ever reaching disk again, on top of every future call to
/// this very function panicking at its own `config_write.lock()`.
///
/// Uses the `#[cfg(test)]` fault-injection seam on `build_serving_client`
/// to make that failure real rather than hypothetical, on a submission
/// guaranteed to resolve Added (an empty fleet, so nothing can match),
/// and asserts BOTH locks survive.
#[test]
fn add_or_update_account_added_path_does_not_poison_either_lock_on_a_client_build_panic() {
    let manager = manager_of(vec![]);
    let submission = account("brand-new@example.com", 0);

    fail_next_client_build();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        manager.add_or_update_account(submission)
    }));
    assert!(
        result.is_err(),
        "setup: the injected client-build failure must actually panic"
    );

    assert!(
        manager.accounts.read().is_ok(),
        "a client-build panic in add_or_update_account must not poison the \
         accounts lock — every other .expect(\"accounts lock poisoned\") \
         call site would panic in turn"
    );
    assert!(
        manager.config_write.lock().is_ok(),
        "a client-build panic in add_or_update_account must not poison the \
         config_write lock either — persist_tokens and persist_now's \
         shutdown flush both .expect() it, and this function itself takes \
         it again on its very next call"
    );
}

/// The Added-only half of the fix above: an ordinary re-auth of an
/// EXISTING account (the common case this route serves — every token
/// refresh's re-login goes through `POST /_tcr/accounts`, not just a
/// first-time add) must resolve Updated on its FIRST locked attempt and
/// never reach `build_serving_client` at all. A version that built the
/// runtime unconditionally before resolving (fixing the poisoning bug
/// above but not this one) would waste a client build on every re-auth
/// and widen the panic surface to a call that never needed one. Arms the
/// SAME fault-injection seam on a submission guaranteed to resolve
/// Updated (an existing row with a matching bare identity) — if the
/// build were ever reached the injected panic would fire, so a normal
/// return proves it was not.
#[test]
fn add_or_update_account_updated_path_never_builds_a_client() {
    let existing = account("alice@example.com", 0);
    let manager = manager_of(vec![existing.clone()]);
    let mut resubmission = existing;
    resubmission.access_token = "at-alice-rotated".to_string();

    fail_next_client_build();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        manager.add_or_update_account(resubmission)
    }));

    let outcome = result.unwrap_or_else(|_| {
        panic!(
            "add_or_update_account must never reach build_serving_client on \
             the Updated path — the injected client-build failure should \
             not have fired at all"
        )
    });
    assert!(
        matches!(outcome, AddAccountOutcome::Updated { .. }),
        "setup: the resubmission must resolve against the existing row, \
         not append a new one"
    );
}

/// The live half of a brand-new-identity race was already serialized by
/// the single `accounts` write-lock spanning resolve+mutate (see the
/// TOCTOU fix above) — but the two callers' DURABLE writes still queued on
/// `config_write` independently, in whatever order the OS scheduled them,
/// with no guarantee that order agreed with which caller actually won the
/// live race. Two real logins of the SAME identity racing each other (a
/// double-submit, a retried `tcr login`) carry genuinely DIFFERENT tokens,
/// so a disagreement is observable: the file could end up holding the
/// LOSING submission's token while the live rotation served the winner's.
/// Measured 2/200 over 200 rounds of two simultaneous adds of one
/// identity before `config_write` was widened to span the whole
/// resolve-mutate-persist sequence. Runs the same 200 rounds to catch it.
#[test]
fn concurrent_adds_of_one_new_identity_never_disagree_with_disk_on_the_token() {
    use std::thread;
    for round in 0..200 {
        let (manager, path) = build_manager_with_disk(
            config_with(vec![account("alice@example.com", 0)]),
            &format!("n2-token-race-{round}"),
        );

        let base = Account {
            account_uuid: Some("uuid-new".to_string()),
            ..account("carol@example.com", 0)
        };
        let submission_a = Account {
            access_token: "at-A".to_string(),
            refresh_token: Some("rt-A".to_string()),
            ..base.clone()
        };
        let submission_b = Account {
            access_token: "at-B".to_string(),
            refresh_token: Some("rt-B".to_string()),
            ..base
        };

        let m1 = Arc::clone(&manager);
        let m2 = Arc::clone(&manager);
        let h1 = thread::spawn(move || m1.add_or_update_account(submission_a));
        let h2 = thread::spawn(move || m2.add_or_update_account(submission_b));
        h1.join().expect("adder A panicked");
        h2.join().expect("adder B panicked");

        let live_token = {
            let accounts = manager.accounts.read().expect("accounts lock poisoned");
            accounts
                .iter()
                .find(|a| a.name == "carol@example.com")
                .expect("carol's row exists live")
                .access_token
                .clone()
        };
        let reloaded = config::load(&path).expect("reload persisted config");
        let disk_token = reloaded
            .accounts
            .iter()
            .find(|a| a.name == "carol@example.com")
            .expect("carol's row exists on disk")
            .access_token
            .clone();

        assert_eq!(
            live_token, disk_token,
            "round {round}: live token ({live_token}) and disk token \
             ({disk_token}) disagree for the same identity"
        );

        std::fs::remove_file(&path).ok();
    }
}

/// F4 — an `Error` row given fresh credentials must clear back to `Active`
/// (`eligible` hard-gates on `status == Error`) but its rate-limit hold must
/// SURVIVE the credential replace — clearing it bought nothing but the
/// ability to race a genuinely-still-limited account back into rotation
/// early. An EXPIRED hold proves both facts at once without contradiction:
/// the field keeps its stamped value, and the account is selectable because
/// that value is already in the past.
#[test]
fn add_or_update_account_clears_error_status_but_keeps_the_rate_limit_hold() {
    let (manager, path) = build_manager_with_disk(
        config_with(vec![account("alice@example.com", 0)]),
        "f4-hold-survives",
    );
    let hold_until = crate::now_ms() - 1_000;
    {
        let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
        accounts[0].status = AccountStatus::Error;
        accounts[0].rate_limited_until_ms = Some(hold_until);
    }

    let submission = Account {
        access_token: "at-fresh".to_string(),
        refresh_token: Some("rt-fresh".to_string()),
        ..account("alice@example.com", 0)
    };
    assert_it_updates_the_first_row(&manager, submission);

    {
        let accounts = manager.accounts.read().expect("accounts lock poisoned");
        assert_eq!(
            accounts[0].status,
            AccountStatus::Active,
            "fresh credentials must clear a stuck error"
        );
        assert_eq!(
            accounts[0].rate_limited_until_ms,
            Some(hold_until),
            "the rate-limit hold must survive a credential replace"
        );
    }

    assert_eq!(
        manager.select(
            &HashSet::new(),
            OffsetDateTime::now_utc(),
            None,
            None,
            "/v1/messages",
            None
        ),
        Some(0),
        "active status + an EXPIRED hold ⇒ immediately selectable"
    );

    std::fs::remove_file(&path).ok();
}

/// F5 hardening. When the durable write must APPEND on `persist_replaced`'s
/// path (no on-disk entry carries this identity — e.g. it was deleted while
/// the proxy ran), the appended record must carry the row's REAL
/// `priority`/`disabled`, never `identity::probe`'s `None` placeholders —
/// otherwise a deliberately-benched account comes back into rotation on the
/// very next restart, the exact bug `persist_disabled` exists to prevent.
#[test]
fn add_or_update_account_persists_real_routing_state_when_appending_via_replace() {
    let alice = Account {
        account_uuid: Some("uuid-alice".to_string()),
        priority: Some(5),
        disabled: Some(true),
        ..account("alice@example.com", 5)
    };
    let (manager, path) = build_manager_with_disk(config_with(vec![alice]), "f5-routing-state");

    // The disk entry vanishes out from under the live process — the durable
    // write must now APPEND.
    std::fs::write(&path, r#"{"accounts": []}"#).expect("rewrite disk config");

    let submission = Account {
        account_uuid: Some("uuid-alice".to_string()),
        access_token: "at-fresh".to_string(),
        refresh_token: Some("rt-fresh".to_string()),
        ..account("alice@example.com", 0)
    };
    assert_it_updates_the_first_row(&manager, submission);

    let reloaded = config::load(&path).expect("reload persisted config");
    assert_eq!(reloaded.accounts.len(), 1);
    assert_eq!(
        reloaded.accounts[0].priority,
        Some(5),
        "a benched account's priority must survive an append-via-replace"
    );
    assert_eq!(
        reloaded.accounts[0].disabled,
        Some(true),
        "…and its disabled flag — the exact restart-un-bench persist_disabled exists to prevent"
    );

    std::fs::remove_file(&path).ok();
}

/// F6 — an update backfills every identity field the row is missing
/// (`account_uuid`/`org_uuid`/`org_name`) and always corrects `account_type`
/// to the submitted value: a row left at a stale non-`"oauth"` type is a
/// silent death, because `refresh_plan` skips any account whose type is not
/// `"oauth"`. On both the live row and the disk entry.
#[test]
fn add_or_update_account_backfills_identity_and_corrects_a_stale_account_type() {
    let legacy_api_row = Account {
        account_type: config::AccountType::Unrecognized("api".to_string()),
        account_uuid: None,
        org_uuid: None,
        org_name: None,
        access_token: "sk-old".to_string(),
        refresh_token: None,
        ..account("alice@example.com", 0)
    };
    let (manager, path) = build_manager_with_disk(config_with(vec![legacy_api_row]), "f6-backfill");

    let submission = Account {
        account_type: config::AccountType::OAuth,
        account_uuid: Some("uuid-alice".to_string()),
        org_uuid: Some("org-corp-uuid".to_string()),
        org_name: Some("Corp".to_string()),
        access_token: "at-fresh".to_string(),
        refresh_token: Some("rt-fresh".to_string()),
        ..account("alice@example.com", 0)
    };
    assert_it_updates_the_first_row(&manager, submission);

    {
        let accounts = manager.accounts.read().expect("accounts lock poisoned");
        assert_eq!(
            accounts[0].account_type,
            config::AccountType::OAuth,
            "a stale non-oauth type must be corrected, or refresh_plan silently \
             never refreshes this account again"
        );
        assert_eq!(accounts[0].account_uuid.as_deref(), Some("uuid-alice"));
        assert_eq!(accounts[0].org_uuid.as_deref(), Some("org-corp-uuid"));
        assert_eq!(accounts[0].org_name.as_deref(), Some("Corp"));
    }

    let reloaded = config::load(&path).expect("reload persisted config");
    assert_eq!(
        reloaded.accounts[0].account_type,
        config::AccountType::OAuth
    );
    assert_eq!(
        reloaded.accounts[0].account_uuid.as_deref(),
        Some("uuid-alice")
    );
    assert_eq!(
        reloaded.accounts[0].org_uuid.as_deref(),
        Some("org-corp-uuid")
    );
    assert_eq!(reloaded.accounts[0].org_name.as_deref(), Some("Corp"));

    std::fs::remove_file(&path).ok();
}

/// Re-adding a Codex account replaces the credential on ITS row. The
/// Updated arm hands `persist_replaced` a target it builds itself, and
/// `config::locate_account_entry` never matches a row of the other
/// provider: a target that defaulted to Anthropic found no row and APPENDED
/// one, an Anthropic row carrying the Codex credential, which the next boot
/// would send to the Anthropic upstream.
///
/// Watched red: give the Updated target `provider: Default::default()` and
/// the file grows a second row with no `provider` key.
#[test]
fn re_adding_a_codex_account_updates_its_row_and_appends_no_anthropic_duplicate() {
    let stored = Account {
        provider: crate::provider::Provider::Codex,
        account_uuid: Some("11111111-1111-4111-8111-111111111111".to_string()),
        access_token: "at-old".to_string(),
        refresh_token: Some("rt-old".to_string()),
        ..account("bob@example.com", 0)
    };
    let (manager, path) = build_manager_with_disk(config_with(vec![stored.clone()]), "codex-readd");

    let submission = Account {
        access_token: "at-fresh".to_string(),
        refresh_token: Some("rt-fresh".to_string()),
        ..stored
    };
    let outcome = manager.add_or_update_account(submission);
    std::assert_matches!(
        outcome,
        AddAccountOutcome::Updated {
            idx: 0,
            persist: AddPersist::Persisted,
            ..
        }
    );

    let after = read_config_json(&path);
    let rows = after["accounts"].as_array().expect("an accounts array");
    assert_eq!(
        rows.len(),
        1,
        "a Codex re-add must update its own row, never append a duplicate: {after}"
    );
    assert_eq!(rows[0]["provider"], serde_json::json!("codex"));
    assert_eq!(rows[0]["refreshToken"], serde_json::json!("rt-fresh"));
    std::fs::remove_file(&path).ok();
}

/// Preservation guard: a submission with NO identity fields at all (a bare
/// name) resolves on NAME EQUALITY alone — `same_identity`'s fallback when
/// either side has no uuid — and a name that matches nothing is APPENDED,
/// never merged onto a row whose name merely looks similar.
///
/// This used to need a `match_one` retry on top, so a bare email could find
/// a stored `alice@example.com (Corp)`. That display-name shape existed only
/// because names were not unique; a submission naming a row that is not
/// there is now a new account, which is the honest reading.
#[test]
fn add_or_update_account_with_a_bare_name_matches_only_that_exact_name() {
    let stored = Account {
        refresh_token: Some("rt-old".to_string()),
        ..account("alice@example.com/corp", 0)
    };
    let (manager, path) = build_manager_with_disk(config_with(vec![stored]), "f-bare-name-exact");

    // The exact stored name updates in place.
    let same_row = Account {
        access_token: "at-fresh".to_string(),
        refresh_token: Some("rt-fresh".to_string()),
        ..account("alice@example.com/corp", 0)
    };
    match manager.add_or_update_account(same_row) {
        AddAccountOutcome::Updated { idx, .. } => assert_eq!(idx, 0),
        other => panic!("an exact name must update its own row: {other:?}"),
    }

    // A different name is a different account, appended.
    let other_row = Account {
        access_token: "at-other".to_string(),
        refresh_token: Some("rt-other".to_string()),
        ..account("alice@example.com", 0)
    };
    match manager.add_or_update_account(other_row) {
        AddAccountOutcome::Added { idx, .. } => assert_eq!(idx, 1),
        other => panic!("an unmatched name must be appended: {other:?}"),
    }
    assert_eq!(
        manager
            .accounts
            .read()
            .expect("accounts lock poisoned")
            .len(),
        2
    );

    std::fs::remove_file(&path).ok();
}
