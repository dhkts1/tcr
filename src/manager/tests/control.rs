//! The lock and control accounts: assembly, routing preference, `set_control_by_query`.

use super::*;

// ---- hard account lock (`lockAccount`; no failover) -----------------------

fn config_with_lock(accounts: Vec<Account>, lock: &str) -> Config {
    let mut config = config_with(accounts);
    config.lock_account = Some(lock.to_string());
    config
}

/// A hard lock pins EVERY select to the locked index, ignoring the LRU
/// preference and any session-affinity key that points elsewhere.
#[test]
fn select_lock_always_returns_locked_idx() {
    let manager = build_manager(
        config_with_lock(
            vec![account("zero", 0), account("one", 0), account("two", 0)],
            "one",
        ),
        counting_refresher(),
    );
    assert_eq!(manager.locked_idx, Some(1));
    let now = OffsetDateTime::now_utc();
    // No affinity, empty tried → locked account regardless of LRU (index 0
    // would be the natural first pick here).
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
        Some(1)
    );
    // An affinity key returns the SAME locked account (lock ignores affinity).
    assert_eq!(
        manager.select(&HashSet::new(), now, None, Some(42), "/v1/messages", None),
        Some(1)
    );
    // Bias the LRU toward index 0 by pinning affinity elsewhere first — lock
    // still wins.
    assert_eq!(
        manager.select(&HashSet::new(), now, None, Some(7), "/v1/messages", None),
        Some(1)
    );
}

/// The lock has NO failover: once the locked account is in `tried`, select
/// returns None rather than rotating to the pool.
#[test]
fn select_lock_returns_none_when_locked_tried() {
    let manager = build_manager(
        config_with_lock(vec![account("zero", 0), account("one", 0)], "one"),
        counting_refresher(),
    );
    assert_eq!(manager.locked_idx, Some(1));
    let now = OffsetDateTime::now_utc();
    let mut tried = HashSet::new();
    tried.insert(1usize);
    assert_eq!(
        manager.select(&tried, now, None, None, "/v1/messages", None),
        None
    );
}

/// `assemble` resolves the configured name to its account index; a name that
/// matches no account resolves to None (runs unlocked).
#[test]
fn assemble_resolves_lock_name() {
    let matched = build_manager(
        config_with_lock(vec![account("a", 0), account("b", 0), account("c", 0)], "c"),
        counting_refresher(),
    );
    assert_eq!(matched.locked_idx, Some(2));

    let unmatched = build_manager(
        config_with_lock(vec![account("a", 0), account("b", 0)], "ghost"),
        counting_refresher(),
    );
    assert_eq!(unmatched.locked_idx, None);
}

/// Absent `lockAccount` → `locked_idx == None` → select is unchanged.
#[test]
fn unlocked_default_leaves_locked_idx_none() {
    let manager = manager_of(vec![account("solo", 0)]);
    assert_eq!(manager.locked_idx, None);
    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
        Some(0)
    );
}

/// `assemble` resolves the configured `controlAccount` name to its account
/// index, mirroring `assemble_resolves_lock_name`; a name that matches no
/// account resolves to `None` (runs with no control account, not a panic).
#[test]
fn assemble_resolves_control_name() {
    let matched = build_manager(
        config_with_control(vec![account("a", 0), account("b", 0), account("c", 0)], "c"),
        counting_refresher(),
    );
    assert_eq!(matched.control(), Some(2));

    let unmatched = build_manager(
        config_with_control(vec![account("a", 0), account("b", 0)], "ghost"),
        counting_refresher(),
    );
    assert_eq!(unmatched.control(), None);
}

/// A `controlAccount` naming a Codex account is warned about and ignored:
/// the control account is an Anthropic identity (its bootstrap calls, its
/// probe, its `controlIdentity` check), and Codex has no counterpart in this
/// version. The same name resolves when the row is Anthropic,
/// so the `None` below is the provider and not a name that never matched.
///
/// Watched red: drop the provider filter after `assemble`'s control
/// resolution and the Codex row resolves (`Some(1)`).
#[test]
fn assemble_ignores_a_codex_control_account() {
    let codex = Account {
        provider: crate::base::provider::Provider::Codex,
        ..account("bob@example.com", 0)
    };
    let manager = build_manager(
        config_with_control(
            vec![account("alice@example.com", 0), codex],
            "bob@example.com",
        ),
        counting_refresher(),
    );
    assert_eq!(
        manager.control(),
        None,
        "a Codex control account must be ignored at boot"
    );

    let anthropic = build_manager(
        config_with_control(
            vec![
                account("alice@example.com", 0),
                account("bob@example.com", 0),
            ],
            "bob@example.com",
        ),
        counting_refresher(),
    );
    assert_eq!(
        anthropic.control(),
        Some(1),
        "the control: an Anthropic row resolves"
    );
}

/// The exact bug this unit fixes: a legacy bare-string `controlAccount`
/// that names a DUPLICATED email must not silently bind to whichever row
/// sorts first (the old `position(...)` behaviour) — it must refuse and run
/// with no control account, same as an unmatched name.
#[test]
fn assemble_refuses_legacy_control_name_on_duplicated_email() {
    let dup_a = Account {
        org_uuid: Some("11111111-1111-1111-1111-111111111111".to_string()),
        ..account("dup@example.com", 0)
    };
    let dup_b = Account {
        org_uuid: Some("22222222-2222-2222-2222-222222222222".to_string()),
        disabled: Some(true),
        ..account("dup@example.com", 0)
    };
    let config = config_with_control(vec![dup_a, dup_b], "dup@example.com");
    let manager = build_manager(config, counting_refresher());
    assert_eq!(
        manager.control(),
        None,
        "an ambiguous legacy name must run with NO control account, never a guess"
    );
}

/// A `controlAccount` written by an OLDER tcr — the identity object, with
/// its org fields — still resolves at boot, by its `name` alone. Nothing
/// writes that shape now, and a config carrying one must still come up on
/// the right row rather than with no control account at all.
///
/// The two rows have distinct names here because `config::load` guarantees
/// that; the object's `orgUuid` is the field this deliberately no longer
/// reads, and the row it names is the one that must win.
#[test]
fn assemble_resolves_a_legacy_identity_control_account_by_name() {
    let personal = account("dup@example.com", 0);
    let team = Account {
        org_uuid: Some("22222222-2222-2222-2222-222222222222".to_string()),
        ..account("dup@example.com/team", 0)
    };
    let mut config = config_with(vec![personal, team]);
    config.control_account = Some(config::ControlAccountRef::Identity {
        name: "dup@example.com/team".to_string(),
        account_uuid: None,
        org_uuid: Some("22222222-2222-2222-2222-222222222222".to_string()),
        org_name: None,
    });
    let manager = build_manager(config, counting_refresher());
    assert_eq!(
        manager.control(),
        Some(1),
        "the object's name must pick the second row, not the first"
    );
}

/// Full round trip through the durable half, on the fleet shape that used
/// to make this impossible: one person, two orgs, one email. The loader
/// gives the second row its own name; setting control by that name persists
/// the NAME (nothing else is needed — a name is unique), and a fresh
/// `Manager` built from the reloaded file resolves to the SAME row, not
/// just "a" row.
///
/// The duplicated-email file is written first and read back through
/// `config::load` deliberately: the migration is the step under test as much
/// as the control write is.
#[test]
fn set_control_on_a_migrated_duplicate_survives_a_reload() {
    let path = tmp_config_path("control-survives-reload");
    let dup_a = Account {
        account_uuid: Some("33333333-3333-3333-3333-333333333333".to_string()),
        org_uuid: Some("11111111-1111-1111-1111-111111111111".to_string()),
        refresh_token: Some("rt-first".to_string()),
        ..account("dup@example.com", 0)
    };
    let dup_b = Account {
        account_uuid: Some("33333333-3333-3333-3333-333333333333".to_string()),
        org_uuid: Some("22222222-2222-2222-2222-222222222222".to_string()),
        refresh_token: Some("rt-second".to_string()),
        ..account("dup@example.com", 0)
    };
    config::save(&path, &config_with(vec![dup_a, dup_b])).expect("write test config");

    // The loader renames the second row — no org name on file, so the name
    // falls back to the first eight characters of its org uuid.
    let migrated = config::load(&path).expect("load migrates the duplicate");
    assert_eq!(migrated.accounts[0].name, "dup@example.com");
    assert_eq!(migrated.accounts[1].name, "dup@example.com/22222222");
    let manager = build_manager_with_path(migrated, path.clone());

    let outcome = manager.set_control_by_query(Some("dup@example.com/22222222"));
    assert_eq!(
        outcome,
        SetControlOutcome::Applied {
            name: Some("dup@example.com/22222222".to_string()),
            persist: ControlPersist::Persisted,
        }
    );
    assert_eq!(manager.control(), Some(1), "resolved the SECOND row live");

    // The bare name is the whole key on disk now.
    let on_disk = config::load(&path).expect("reload persisted config");
    assert_eq!(
        on_disk.control_account,
        Some(config::ControlAccountRef::Name(
            "dup@example.com/22222222".to_string()
        ))
    );
    assert!(
        on_disk.renamed_accounts.is_empty(),
        "the second load has nothing left to rename"
    );

    // A brand-new Manager built from the reloaded file resolves to the
    // SAME row (index 1, `rt-second`) — not just "a" row.
    let reloaded_manager = build_manager_with_path(on_disk, path.clone());
    assert_eq!(reloaded_manager.control(), Some(1));

    std::fs::remove_file(&path).ok();
}

/// Absent `controlAccount` → `control() == None`.
#[test]
fn control_absent_default_leaves_control_none() {
    let manager = manager_of(vec![account("solo", 0)]);
    assert_eq!(manager.control(), None);
}

/// PART 1's invariant, NARROWED by part 2 (deliberately — see the module
/// doc for the three-way split): setting a control account must not change
/// which account `select` returns **for inference traffic on the realistic
/// deployment shape, where the control account stays `disabled`** (the
/// documented default — see `Manager::control_idx`'s doc). Part 1 asserted
/// this unconditionally, because part 1 shipped no routing at all and the
/// control account was inert everywhere.
///
/// That is no longer strictly true in general: part 2 permanently excludes
/// the control account from inference candidacy REGARDLESS of `disabled`
/// (see [`inference_never_goes_to_control_even_when_unpinned`], which
/// deliberately leaves control ENABLED and asserts inference still skips
/// it) — so on a tiny fleet where the enabled control account would
/// otherwise have been the natural LRU pick, inference selection DOES
/// change the moment `disabled` is lifted. This test is narrowed to the
/// disabled case (`set_disabled(0, true)` below) where the exclusion is
/// provably redundant with `eligible`'s own disabled check, so the
/// original "byte-identical" claim keeps holding exactly where it matters
/// operationally. [`control_preference_prefers_control_for_non_inference_paths`]
/// covers the (expected) non-inference side changing.
#[test]
fn setting_a_control_account_does_not_change_selection() {
    let without_control = manager_of(vec![account("a", 0), account("b", 0)]);
    let with_control = build_manager(
        config_with_control(vec![account("a", 0), account("b", 0)], "a"),
        counting_refresher(),
    );
    // Realistic deployment shape: the control account stays `disabled` —
    // see the doc-comment above for why this is what keeps the invariant
    // honestly true rather than coincidentally true. Disabled on BOTH
    // managers, isolating the effect of `controlAccount` itself: the
    // baseline already excludes account 0 the ordinary way (a plain
    // `disabled` account, unrelated to control), so any DIFFERENCE
    // between the two would be attributable to the control preference,
    // not to `disabled` doing double duty.
    without_control.set_disabled(0, true);
    with_control.set_disabled(0, true);
    assert_eq!(with_control.control(), Some(0));

    let now = OffsetDateTime::now_utc();
    for _ in 0..20 {
        let picked_without =
            without_control.select(&HashSet::new(), now, None, None, "/v1/messages", None);
        let picked_with =
            with_control.select(&HashSet::new(), now, None, None, "/v1/messages", None);
        assert_eq!(
            picked_without, picked_with,
            "a control account must not change which account select() returns for inference traffic"
        );
        if let Some(idx) = picked_without {
            without_control.set_current(idx);
            with_control.set_current(idx);
        }
    }
}

/// The other half of the narrowing above: an UNPINNED, non-inference,
/// non-noise request (the identity/control plane) DOES change the pick —
/// it now prefers the control account, byte-different from `without_control`.
#[test]
fn control_preference_prefers_control_for_non_inference_paths() {
    let without_control = manager_of(vec![account("a", 0), account("b", 0)]);
    let with_control = build_manager(
        config_with_control(vec![account("a", 0), account("b", 0)], "b"),
        counting_refresher(),
    );
    assert_eq!(with_control.control(), Some(1));

    let now = OffsetDateTime::now_utc();
    let picked_without =
        without_control.select(&HashSet::new(), now, None, None, "/api/organizations", None);
    let picked_with =
        with_control.select(&HashSet::new(), now, None, None, "/api/organizations", None);
    assert_eq!(
        picked_without,
        Some(0),
        "no control account: ordinary LRU picks index 0"
    );
    assert_eq!(
        picked_with,
        Some(1),
        "a control account IS preferred for an unpinned identity-plane request"
    );
}

// ---- control-account routing (part 2 — see docs/plans/control-routing-bridge-coder.md) ----

/// Invariant 1, the one that matters: an EXISTING pin is never re-keyed by
/// the control preference, even after control is set at runtime.
#[test]
fn control_preference_never_moves_an_existing_pin() {
    let manager = manager_of(vec![account("pool", 0), account("ctrl", 0)]);
    let now = OffsetDateTime::now_utc();
    let key = 555u64;
    // Pin session `key` to account 0 (pool) BEFORE any control account
    // exists — an ordinary identity-plane select, no preference in play.
    let pinned = manager
        .select(
            &HashSet::new(),
            now,
            None,
            Some(key),
            "/api/organizations",
            None,
        )
        .expect("an account is eligible");
    assert_eq!(pinned, 0);

    // Set the control account to `ctrl` (index 1) at runtime.
    manager.set_control_by_query(Some("ctrl"));
    assert_eq!(manager.control(), Some(1));

    // The SAME session's next identity-plane request must still return its
    // EXISTING pin (0), never the newly-preferred control account (1).
    for _ in 0..5 {
        assert_eq!(
            manager.select(
                &HashSet::new(),
                now,
                None,
                Some(key),
                "/api/organizations",
                None
            ),
            Some(0),
            "an existing pin must never be re-keyed by the control preference"
        );
    }
}

/// The `keep_pin` half of invariant 1, distinct from the test above: a pin
/// that is only DIVERTED for one request (it already failed upstream —
/// `tried` — but the ACCOUNT itself is still alive) must not be re-keyed to
/// the control account either. The test above never actually reaches the
/// control overlay (a clean, servable pin returns from the affinity
/// fast-path first); this one forces a divert so `keep_pin` really is
/// `Some(_)` when the overlay's `keep_pin.is_none()` gate is checked.
#[test]
fn control_preference_never_moves_a_diverted_pin() {
    let manager = manager_of(vec![account("a", 0), account("b", 0), account("c", 0)]);
    let now = OffsetDateTime::now_utc();
    let key = 777u64;
    let pinned = manager
        .select(
            &HashSet::new(),
            now,
            None,
            Some(key),
            "/api/organizations",
            None,
        )
        .expect("an account is eligible");
    assert_eq!(pinned, 0, "the first pick, tie-broken by index, is \"a\"");

    // Set control to "c" (index 2) — a DIFFERENT account — after the pin
    // already exists.
    manager.set_control_by_query(Some("c"));
    assert_eq!(manager.control(), Some(2));

    // Bias ordinary LRU firmly toward "b" (index 1): stamp "c" (control)
    // as ALREADY selected, so its LRU key sorts strictly AFTER "b"'s
    // never-selected key. If the divert incorrectly routed straight to
    // control (the hoisting bug this test exists to catch — the overlay
    // reached ahead of the `keep_pin` gate), it would land on "c"
    // regardless of this bias, because it never consults ordinary LRU at
    // all for a control-preferred pick.
    {
        let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
        accounts[2].last_selected_seq = 999;
    }

    // This ONE request diverts — the pinned account already failed it.
    let mut tried = HashSet::new();
    tried.insert(pinned);
    let served = manager
        .select(&tried, now, None, Some(key), "/api/organizations", None)
        .expect("a divert target is eligible");
    assert_eq!(
        served, 1,
        "a diverted pinned request must go through ORDINARY rotation (picking \"b\"), \
         not straight to the control account"
    );

    // A clean retry must still return the ORIGINAL pin, never re-keyed to
    // the control account by the divert.
    let rechecked = manager
        .select(
            &HashSet::new(),
            now,
            None,
            Some(key),
            "/api/organizations",
            None,
        )
        .expect("an account is eligible");
    assert_eq!(
        rechecked, pinned,
        "a per-request divert must never re-key the session's pin to the control account"
    );
}

/// §1 no-op guard: the control account ships `disabled` (out of the
/// inference rotation, by design — see the module doc), and the control
/// preference must BYPASS that gate. If `select` used `eligible` or
/// `account_hard_ok` here instead of `Self::control_eligible`, this test
/// fails silently useful: it would still compile and the feature would
/// simply never fire (the single most likely way to get this wrong).
#[test]
fn control_is_picked_even_though_disabled() {
    let manager = build_manager(
        config_with_control(vec![account("pool", 0), account("ctrl", 0)], "ctrl"),
        counting_refresher(),
    );
    manager.set_disabled(1, true);
    assert_eq!(manager.control(), Some(1));
    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, "/api/organizations", None),
        Some(1),
        "a disabled control account must still be picked for an identity-plane request"
    );
}

#[test]
fn control_is_not_picked_when_errored() {
    let manager = build_manager(
        config_with_control(vec![account("pool", 0), account("ctrl", 0)], "ctrl"),
        counting_refresher(),
    );
    manager.set_disabled(1, true);
    {
        let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
        accounts[1].status = AccountStatus::Error;
    }
    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, "/api/organizations", None),
        Some(0),
        "an ERRORED control account must degrade to normal rotation"
    );
}

#[test]
fn control_is_not_picked_when_rejected() {
    let manager = build_manager(
        config_with_control(vec![account("pool", 0), account("ctrl", 0)], "ctrl"),
        counting_refresher(),
    );
    manager.set_disabled(1, true);
    {
        let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
        accounts[1].quota.status = Some("rejected".to_string());
    }
    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, "/api/organizations", None),
        Some(0),
        "a REJECTED control account must degrade to normal rotation"
    );
}

#[test]
fn control_is_not_picked_when_rate_limited() {
    let manager = build_manager(
        config_with_control(vec![account("pool", 0), account("ctrl", 0)], "ctrl"),
        counting_refresher(),
    );
    manager.set_disabled(1, true);
    let now = OffsetDateTime::now_utc();
    let now_ms = odt_to_ms(now);
    {
        let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
        accounts[1].rate_limited_until_ms = Some(now_ms + 60_000);
    }
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, "/api/organizations", None),
        Some(0),
        "a LIVE-held control account must degrade to normal rotation"
    );
}

/// Inference must NEVER select the control account — including UNPINNED
/// and with control left ENABLED (pooled), where `eligible` alone would
/// happily pick it. Only the dedicated pool-pick exclusion stops it.
#[test]
fn inference_never_goes_to_control_even_when_unpinned() {
    let manager = build_manager(
        config_with_control(vec![account("ctrl", 0), account("pool", 0)], "ctrl"),
        counting_refresher(),
    );
    assert_eq!(manager.control(), Some(0));
    let now = OffsetDateTime::now_utc();
    for _ in 0..10 {
        assert_eq!(
            manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
            Some(1),
            "inference must route to the pool, never the control account"
        );
    }
}

/// `controlPooled` lifts the exclusion the test above pins down: the same
/// unpinned inference now reaches the control account, because
/// `select_with_group` stops force-adding the control index to `tried`.
/// Asserts REACHABILITY, not a fixed index — LRU decides the order, and
/// pinning the order here would be testing the tiebreak, not the flag.
#[test]
fn pooled_control_account_takes_inference() {
    let mut config = config_with_control(vec![account("ctrl", 0), account("pool", 0)], "ctrl");
    config.control_pooled = true;
    let manager = build_manager(config, counting_refresher());
    assert_eq!(manager.control(), Some(0));
    let now = OffsetDateTime::now_utc();
    let reached_control = (0..10)
        .any(|_| manager.select(&HashSet::new(), now, None, None, "/v1/messages", None) == Some(0));
    assert!(
        reached_control,
        "controlPooled must let an inference pick reach the control account"
    );
}

/// The guard the whole opt-in rests on, and the reason `controlPooled` is
/// not simply "delete the exclusion": a POOLED control account is held out
/// at `switchThreshold - controlReserve` (0.90 - 0.05 = 0.85), NOT at the
/// full threshold every other account gets.
///
/// 0.86 is chosen to sit BELOW the full 0.90 on purpose — `eligible` still
/// passes this account, so the only thing that can hold it back is
/// [`super::select::Manager::pool_pick_respects_control_reserve`]. Raise it
/// to 0.90 and the test would pass for the wrong reason.
#[test]
fn pooled_control_account_is_held_out_by_the_reserve() {
    let mut config = config_with_control(vec![account("ctrl", 0), account("pool", 0)], "ctrl");
    config.control_pooled = true;
    let manager = build_manager(config, counting_refresher());
    let now = OffsetDateTime::now_utc();
    {
        let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
        accounts[0].quota.five_hour = Some(crate::records::quota::QuotaWindow {
            utilization: 0.86,
            reset: Some(now + Duration::seconds(300)),
        });
    }
    for _ in 0..10 {
        assert_eq!(
            manager.select(&HashSet::new(), now, None, None, "/v1/messages", None),
            Some(1),
            "a pooled control account past threshold-reserve must be held out, \
             leaving the identity plane the headroom the reserve exists to keep"
        );
    }
}

/// §2: a NOISE request reuses whichever account this connection already
/// served.
#[test]
fn noise_path_follows_its_connection() {
    let manager = manager_of(vec![account("a", 0), account("b", 0), account("c", 0)]);
    let now = OffsetDateTime::now_utc();
    let conn = 42u64;
    let first = manager
        .select(
            &HashSet::new(),
            now,
            None,
            None,
            "/api/event_logging/v2/batch",
            Some(conn),
        )
        .expect("an account is eligible");
    for _ in 0..5 {
        assert_eq!(
            manager.select(
                &HashSet::new(),
                now,
                None,
                None,
                "/api/event_logging/v2/batch",
                Some(conn)
            ),
            Some(first),
            "noise traffic on the same connection must stay on the same account"
        );
    }
}

/// §2 fallback: once the connection's account is no longer eligible,
/// normal rotation takes over AND `conn_affinity` is re-recorded to the
/// new winner (not left stale on the held account).
#[test]
fn noise_path_falls_back_when_its_account_is_held() {
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    let now = OffsetDateTime::now_utc();
    let now_ms = odt_to_ms(now);
    let conn = 7u64;
    let first = manager
        .select(
            &HashSet::new(),
            now,
            None,
            None,
            "/mcp-registry/list",
            Some(conn),
        )
        .expect("an account is eligible");
    {
        let mut accounts = manager.accounts.write().expect("accounts lock poisoned");
        accounts[first].rate_limited_until_ms = Some(now_ms + 60_000);
    }
    let second = manager
        .select(
            &HashSet::new(),
            now,
            None,
            None,
            "/mcp-registry/list",
            Some(conn),
        )
        .expect("the other account is still eligible");
    assert_ne!(
        second, first,
        "a held connection-pinned account must fall back to rotation"
    );
    assert_eq!(
        manager.select(
            &HashSet::new(),
            now,
            None,
            None,
            "/mcp-registry/list",
            Some(conn)
        ),
        Some(second),
        "conn_affinity must be RE-RECORDED to the new winner, not left stale"
    );
}

/// Proves the split splits: noise and identity traffic on the SAME
/// connection route independently — a noise pin to the pool account must
/// not short-circuit the identity plane's control preference.
#[test]
fn identity_path_still_goes_to_control_on_the_same_connection() {
    let manager = build_manager(
        config_with_control(vec![account("pool", 0), account("ctrl", 0)], "ctrl"),
        counting_refresher(),
    );
    manager.set_disabled(1, true);
    let now = OffsetDateTime::now_utc();
    let conn = 99u64;
    assert_eq!(
        manager.select(
            &HashSet::new(),
            now,
            None,
            None,
            "/api/event_logging/v2/batch",
            Some(conn)
        ),
        Some(0),
        "noise pins this connection to the pool account"
    );
    assert_eq!(
        manager.select(
            &HashSet::new(),
            now,
            None,
            None,
            "/api/organizations",
            Some(conn)
        ),
        Some(1),
        "control preference must not be short-circuited by the connection's noise pin"
    );
}

/// `conn_affinity` is a SEPARATE, memory-only map (invariant 3): noise
/// traffic must never mark the persisted affinity map dirty or write into
/// it, mirroring `Self::affinity`'s own persistence contract.
#[test]
fn conn_affinity_is_never_persisted() {
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    assert!(!manager.affinity_file.is_dirty());
    let now = OffsetDateTime::now_utc();
    for i in 0..5u64 {
        manager.select(
            &HashSet::new(),
            now,
            None,
            None,
            "/api/event_logging/v2/batch",
            Some(i),
        );
    }
    assert!(
        !manager.affinity_file.is_dirty(),
        "noise traffic must never mark the persisted affinity map dirty"
    );
    assert!(
        manager
            .affinity
            .lock()
            .expect("affinity lock poisoned")
            .is_empty(),
        "noise traffic must never write into the persisted affinity map"
    );
}

/// Invariant 4: inert with no control account set — path classification
/// changes nothing about ordinary LRU.
#[test]
fn control_preference_is_inert_when_unset() {
    let manager = manager_of(vec![account("a", 0), account("b", 0)]);
    assert_eq!(manager.control(), None);
    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, "/api/organizations", None),
        Some(0),
        "no control account set -> ordinary LRU, unaffected by path classification"
    );
}

/// The hard account lock still short-circuits BEFORE path classification
/// is even computed — control preference can never outrank it.
#[test]
fn control_preference_never_outranks_lock_account() {
    let mut config = config_with_control(vec![account("locked", 0), account("ctrl", 0)], "ctrl");
    config.lock_account = Some("locked".to_string());
    let manager = build_manager(config, counting_refresher());
    assert_eq!(manager.locked_idx, Some(0));
    assert_eq!(manager.control(), Some(1));
    let now = OffsetDateTime::now_utc();
    assert_eq!(
        manager.select(&HashSet::new(), now, None, None, "/api/organizations", None),
        Some(0),
        "the hard lock must win over control preference regardless of path"
    );
}

/// `set_control_by_query` resolves against the LIVE rotation (mirroring
/// `set_disabled_by_query`), reports the resolved name, sets `control()`,
/// and a `None` query clears it.
#[test]
fn set_control_by_query_resolves_sets_and_clears() {
    let manager = manager_of(vec![account("gil", 0), account("other", 0)]);

    let outcome = manager.set_control_by_query(Some("gil"));
    assert_eq!(
        outcome,
        SetControlOutcome::Applied {
            name: Some("gil".to_string()),
            persist: ControlPersist::NoConfigFile,
        }
    );
    assert_eq!(manager.control(), Some(0));
    assert_eq!(manager.control_name(), Some("gil".to_string()));

    let cleared = manager.set_control_by_query(None);
    assert_eq!(
        cleared,
        SetControlOutcome::Applied {
            name: None,
            persist: ControlPersist::NoConfigFile,
        }
    );
    assert_eq!(manager.control(), None);
}

/// `tcr control` naming a Codex account is refused at runtime, as boot ignores one
/// (`assemble_ignores_a_codex_control_account`): nothing is set, so nothing is
/// persisted, and a control account already in place stays. The Anthropic row of
/// the same fleet is set, so the refusal is the provider's and not a name that
/// never matched.
///
/// Watched red: drop the provider check from `set_control_by_query` and the Codex
/// row is applied.
#[test]
fn set_control_by_query_refuses_a_codex_account() {
    let codex = Account {
        provider: crate::base::provider::Provider::Codex,
        ..account("bob@example.com/codex", 0)
    };
    let manager = manager_of(vec![account("alice@example.com", 0), codex]);
    let refused = SetControlOutcome::CodexAccount("bob@example.com/codex".to_string());
    assert_eq!(
        manager.set_control_by_query(Some("bob@example.com/codex")),
        refused
    );
    assert_eq!(manager.control(), None, "nothing is set");

    manager.set_control_by_query(Some("alice@example.com"));
    assert_eq!(
        manager.control(),
        Some(0),
        "control: the Anthropic row is set"
    );
    assert_eq!(
        manager.set_control_by_query(Some("bob@example.com/codex")),
        refused
    );
    assert_eq!(
        manager.control(),
        Some(0),
        "the control account already in place stays"
    );
}

#[test]
fn set_control_by_query_no_match_and_ambiguous() {
    let manager = manager_of(vec![account("dup", 0), account("dup", 0)]);
    assert_eq!(
        manager.set_control_by_query(Some("ghost")),
        SetControlOutcome::NoMatch
    );
    assert_eq!(
        manager.set_control_by_query(Some("dup")),
        SetControlOutcome::Ambiguous(vec!["dup".to_string(), "dup".to_string()])
    );
    assert_eq!(
        manager.control(),
        None,
        "an ambiguous query must not set anything"
    );
}
