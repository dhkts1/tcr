//! Writing account state back to the config file: rotated tokens, disable toggles, outcomes.

use super::*;

/// The other half, and the one that needs no request at all to fire:
/// `ServerHandle::shutdown_within` ends by calling `persist_now`, so an
/// ungated predecessor writes its stale in-memory tokens over the
/// successor's fresh ones as its very last act.
#[tokio::test]
async fn a_released_manager_does_not_write_the_config() {
    let path = tmp_config_path("released-no-persist");
    let manager = build_manager_with_path(config_with(vec![account("a", 0)]), path.clone());

    // A persist while ownership is held establishes the file and proves the
    // write path is live -- without this, an unchanged file below could just
    // mean nothing ever writes here.
    manager.persist_now();
    let owned = std::fs::read(&path).expect("the owned persist must have written the file");
    assert!(
        !owned.is_empty(),
        "precondition: the owned persist wrote bytes"
    );

    // Stand in for the successor having rewritten the file after the handoff.
    std::fs::write(&path, b"{\"successor\":true}").expect("writing the successor's file");

    manager.release_mutation_ownership();
    manager.persist_now();

    assert_eq!(
        std::fs::read(&path).expect("reading the file back"),
        b"{\"successor\":true}",
        "a released manager overwrote the successor's config file"
    );
}

/// `persist_tokens` must write ONLY the account it rotated.
///
/// `config::CredentialScope` makes that expressible; this is the test that
/// it is USED. Every config-level test passes with `persist_tokens` handing
/// over `CredentialScope::All`, so without this the fix could be reverted
/// and nothing would go red.
///
/// The story: the manager holds a boot-time snapshot. Another `tcr` process
/// re-auths `acct-b`, putting a fresh single-use refresh token on disk. The
/// manager then rotates `acct-a` and persists. Its snapshot still carries
/// b's OLD credential, and writing it would leave b dead until re-authed by
/// hand. Strictly ordered, no concurrency, which is why a file lock is not
/// the fix here.
#[test]
fn a_rotation_persists_only_the_rotated_account() {
    let path = tmp_config_path("scoped-persist");
    let config = config_with(vec![account("a", 0), account("b", 1)]);
    let manager = build_manager_with_path(config, path.clone());

    // Establish the file from the manager's own snapshot, so an unchanged
    // value below cannot just mean nothing ever writes here.
    manager.persist_now();
    let before = std::fs::read_to_string(&path).expect("the persist must write the file");
    assert!(
        before.contains("rt-b"),
        "precondition: acct-b's original credential is on disk: {before}"
    );

    // Another process re-auths acct-b. Completes before the rotation below.
    let mut relogin = config::load(&path).expect("the file we just wrote loads");
    relogin.accounts[1].refresh_token = Some("rt-b-relogin".to_string());
    config::save_tokens(&path, &relogin).expect("the re-auth write");

    // The manager rotates acct-a only.
    manager.persist_tokens(
        "a",
        None,
        None,
        None,
        crate::provider::Provider::Anthropic,
        &Tokens {
            id_token: None,
            access_token: "at-a-rotated".to_string(),
            refresh_token: Some("rt-a-rotated".to_string()),
            expires_at_ms: crate::now_ms() + 3_600_000,
        },
    );

    let value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("read back"))
            .expect("the config stays valid JSON");
    assert_eq!(
        value["accounts"][0]["refreshToken"],
        serde_json::json!("rt-a-rotated"),
        "the rotation this persist exists for must land"
    );
    assert_eq!(
        value["accounts"][1]["refreshToken"],
        serde_json::json!("rt-b-relogin"),
        "rotating acct-a reverted acct-b's re-auth; that token is single-use \
         and acct-b is now dead until `tcr login`"
    );

    std::fs::remove_file(&path).ok();
}

/// A rotated Codex credential is persisted onto the Codex row, and the Anthropic
/// row that shares its name keeps its own.
///
/// Neither row carries an account UUID, so identity alone falls back to the name
/// and calls the two rows one account; only the provider tells them apart. Both
/// halves of the write depend on it: provider-blind, `persist_tokens`' own
/// placement ties and refuses, and so does the merge that pairs on-disk entries
/// with loaded accounts — either way the rotated single-use refresh token never
/// reaches the file, and the Codex account is dead after the next restart.
#[test]
fn a_rotated_codex_token_persists_onto_the_codex_row_beside_a_same_named_anthropic_row() {
    let path = tmp_config_path("codex-beside-anthropic");
    let mut anthropic = account("alice@example.com", 0);
    anthropic.access_token = "at-anthropic".to_string();
    anthropic.refresh_token = Some("rt-anthropic".to_string());
    let mut codex = account("alice@example.com", 1);
    codex.provider = crate::provider::Provider::Codex;
    codex.access_token = "at-codex".to_string();
    codex.refresh_token = Some("rt-codex".to_string());
    let manager = build_manager_with_path(config_with(vec![anthropic, codex]), path.clone());

    // Establish the file from the manager's own snapshot, so an unchanged row
    // below cannot just mean nothing ever writes here.
    manager.persist_now();

    manager.persist_tokens(
        "alice@example.com",
        None,
        None,
        None,
        crate::provider::Provider::Codex,
        &Tokens {
            id_token: None,
            access_token: "at-codex-rotated".to_string(),
            refresh_token: Some("rt-codex-rotated".to_string()),
            expires_at_ms: crate::now_ms() + 3_600_000,
        },
    );

    let value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("read back"))
            .expect("the config stays valid JSON");
    let row_of = |provider: Option<&str>| {
        value["accounts"]
            .as_array()
            .expect("an accounts array")
            .iter()
            .find(|row| row.get("provider").and_then(serde_json::Value::as_str) == provider)
            .unwrap_or_else(|| panic!("no row with provider {provider:?}: {value}"))
            .clone()
    };
    let anthropic_row = row_of(None);
    assert_eq!(
        anthropic_row["refreshToken"],
        serde_json::json!("rt-anthropic"),
        "a Codex rotation must leave the Anthropic row's own credential alone: {value}"
    );
    assert_eq!(
        anthropic_row["accessToken"],
        serde_json::json!("at-anthropic")
    );
    let codex_row = row_of(Some("codex"));
    assert_eq!(
        codex_row["refreshToken"],
        serde_json::json!("rt-codex-rotated"),
        "the rotated Codex token must reach the Codex row: {value}"
    );
    assert_eq!(
        codex_row["accessToken"],
        serde_json::json!("at-codex-rotated")
    );

    std::fs::remove_file(&path).ok();
}

/// Released is terminal. Nothing clears it, and releasing twice is safe --
/// both `shutdown` and `Drop` can reach the same path.
#[test]
fn releasing_mutation_ownership_is_terminal_and_idempotent() {
    let manager = manager_of(vec![account("a", 0)]);
    assert!(!manager.mutation_is_released(), "starts owning mutation");
    manager.release_mutation_ownership();
    assert!(manager.mutation_is_released());
    manager.release_mutation_ownership();
    assert!(
        manager.mutation_is_released(),
        "a second release must not toggle it back"
    );
}

/// Regression: `persist_tokens` must save the config UNDER the lock, or two
/// concurrent token refreshes race on the file and one account's just-rotated
/// refresh token is lost — that account then 400s on its next refresh. Fire N
/// concurrent persists for distinct accounts and assert the reloaded file has
/// EVERY account's new refresh token.
#[test]
fn concurrent_token_persists_do_not_lose_updates() {
    use std::thread;
    let tmp = std::env::temp_dir().join(format!(
        "tcr-persist-race-{}-{}.json",
        std::process::id(),
        crate::now_ms()
    ));
    let n = 8usize;
    let accts: Vec<Account> = (0..n).map(|i| account(&format!("a{i}"), 0)).collect();

    let manager = Manager::new(
        config_with(accts),
        counting_refresher(),
        Arc::new(NoProber),
        Arc::new(NoWarmer),
        Some(tmp.clone()),
    );

    let handles: Vec<_> = (0..n)
        .map(|i| {
            let m = Arc::clone(&manager);
            thread::spawn(move || {
                m.persist_tokens(
                    &format!("a{i}"),
                    None,
                    None,
                    None,
                    crate::provider::Provider::Anthropic,
                    &Tokens {
                        id_token: None,
                        access_token: format!("new-at-{i}"),
                        refresh_token: Some(format!("new-rt-{i}")),
                        expires_at_ms: crate::now_ms() + 3_600_000,
                    },
                );
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }

    let reloaded = config::load(&tmp).expect("reload persisted config");
    for i in 0..n {
        let a = reloaded
            .accounts
            .iter()
            .find(|a| a.name == format!("a{i}"))
            .expect("account present after concurrent persist");
        assert_eq!(
            a.refresh_token.as_deref(),
            Some(format!("new-rt-{i}").as_str()),
            "account a{i} lost its rotated refresh token under concurrent persist"
        );
    }
    std::fs::remove_file(&tmp).ok();
}

// ---- durable disable (the TUI's `d`/`e` survives a restart) ----

/// A config file for `accounts`, in the on-disk shape, carrying an unmodelled
/// top-level key so collateral damage is visible.
fn write_account_file(path: &std::path::Path, names: &[&str]) {
    let entries: Vec<String> = names
        .iter()
        .map(|n| {
            format!(
                r#"{{ "name": "{n}", "type": "oauth", "accessToken": "at-{n}", "refreshToken": "rt-{n}" }}"#
            )
        })
        .collect();
    std::fs::write(
        path,
        format!(
            r#"{{ "warmupSeconds": 900, "accounts": [ {} ] }}"#,
            entries.join(", ")
        ),
    )
    .expect("write test config");
}

/// THE regression guard for this fix. Benching an account from the TUI wrote
/// memory only, so a restart silently returned it to rotation. `set_disabled`
/// must reach the file — and `e` must remove the key again, not write `false`.
#[test]
fn set_disabled_persists_through_to_the_config_file() {
    let path = tmp_config_path("durable-disable");
    write_account_file(&path, &["acct-a", "acct-b"]);
    let manager = build_manager_with_path(
        config_with(vec![account("acct-a", 0), account("acct-b", 0)]),
        path.clone(),
    );

    manager.set_disabled(0, true);

    let after = read_config_json(&path);
    assert_eq!(after["accounts"][0]["disabled"], serde_json::json!(true));
    assert!(
        after["accounts"][1].get("disabled").is_none(),
        "the unrelated account was flagged too: {after}"
    );
    assert_eq!(
        after["warmupSeconds"],
        serde_json::json!(900),
        "an unmodelled key was dropped by the flag write"
    );
    // The in-memory config must agree with the file, so the two views of the
    // flag cannot diverge.
    assert_eq!(
        manager.config.lock().unwrap().accounts[0].disabled,
        Some(true)
    );

    // Re-enabling DROPS the key (the CLI contract), never writes false.
    manager.set_disabled(0, false);
    let after = read_config_json(&path);
    assert!(
        after["accounts"][0].get("disabled").is_none(),
        "re-enable must drop the key, not write false: {after}"
    );
    assert_eq!(manager.config.lock().unwrap().accounts[0].disabled, None);
    std::fs::remove_file(&path).ok();
}

/// A Codex account's bench reaches ITS row. `config::locate_account_entry`
/// never matches a row of the other provider, so the probe `set_disabled`
/// locates the row by must carry the runtime account's provider: built as
/// Anthropic it addresses nothing, and the bench silently does not survive a
/// restart.
///
/// Watched red: build `set_disabled`'s probe with `Provider::Anthropic` in
/// place of `account.provider` and the outcome is `NoEntry`, with the Codex
/// row unflagged.
#[test]
fn set_disabled_on_a_codex_account_persists_to_its_codex_row() {
    let codex = Account {
        provider: crate::provider::Provider::Codex,
        ..account("bob@example.com", 0)
    };
    let (manager, path) = build_manager_with_disk(
        config_with(vec![account("alice@example.com", 0), codex]),
        "codex-disable",
    );

    assert_eq!(
        manager.set_disabled(1, true),
        DisablePersist::Persisted,
        "the Codex account's bench must reach its own row"
    );
    let after = read_config_json(&path);
    assert_eq!(after["accounts"][1]["disabled"], serde_json::json!(true));
    assert_eq!(after["accounts"][1]["provider"], serde_json::json!("codex"));
    assert!(
        after["accounts"][0].get("disabled").is_none(),
        "the Anthropic row must be untouched: {after}"
    );
    std::fs::remove_file(&path).ok();
}

/// A manager with NO `config_path` — every test here, `tcr demo`, and
/// `tcr status --probe` — must write nothing at all. The demo drives
/// `set_disabled` on rows that have no config file behind them.
///
/// Differential against the test above: same fixture, same call, and the only
/// difference is the absent path. Without the pair, "the file did not change"
/// would prove nothing.
#[test]
fn set_disabled_writes_nothing_without_a_config_path() {
    let path = tmp_config_path("durable-disable-none");
    write_account_file(&path, &["acct-a"]);
    let before = std::fs::read_to_string(&path).unwrap();

    let manager = manager_of(vec![account("acct-a", 0)]);
    assert!(manager.config_path.is_none());

    manager.set_disabled(0, true);

    // The runtime row still flips — only the disk write is suppressed.
    assert!(manager.snapshot(OffsetDateTime::now_utc()).accounts[0].disabled);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        before,
        "a manager with no config_path must never write to disk"
    );
    std::fs::remove_file(&path).ok();
}

/// **INV3 / FIX 4.** A write that did not land must leave the in-memory snapshot
/// alone. Mutating memory first and regardless of the outcome left the two views
/// permanently diverged — memory saying benched, disk saying nothing — with
/// nothing to reconcile or retry them, which is the opposite of the guarantee the
/// comment above `persist_disabled` claimed.
///
/// All three failure shapes, because each takes a different arm:
/// `NoEntry` (identity absent from disk), `Ambiguous` (two entries share it), and
/// `Err` (the file cannot even be parsed).
#[test]
fn a_failed_persist_leaves_the_in_memory_snapshot_unchanged() {
    // NoEntry — the runtime row exists, the disk entry does not.
    let path = tmp_config_path("persist-noentry");
    write_account_file(&path, &["someone-else"]);
    let manager = build_manager_with_path(config_with(vec![account("acct-a", 0)]), path.clone());
    assert_eq!(
        manager.set_disabled(0, true),
        DisablePersist::NoEntry,
        "no disk entry carries this identity"
    );
    assert_eq!(
        manager.config.lock().unwrap().accounts[0].disabled,
        None,
        "a write that never landed must not be reflected in memory"
    );
    // The runtime row still flips — the bench takes effect NOW, it just is not
    // durable, which is exactly what the caller is told. Read off the row
    // itself, not a snapshot: this file never held `acct-a`, so a snapshot's
    // reload marks the row removed and lists nothing.
    assert!(manager.accounts.read().unwrap()[0].disabled);
    std::fs::remove_file(&path).ok();

    // Ambiguous — two disk entries share the identity, so none may be chosen.
    let path = tmp_config_path("persist-ambiguous");
    write_account_file(&path, &["acct-a", "acct-a"]);
    let manager = build_manager_with_path(config_with(vec![account("acct-a", 0)]), path.clone());
    let before = std::fs::read_to_string(&path).unwrap();
    assert_eq!(manager.set_disabled(0, true), DisablePersist::Ambiguous);
    assert_eq!(manager.config.lock().unwrap().accounts[0].disabled, None);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        before,
        "an ambiguous identity must leave the file byte-identical"
    );
    std::fs::remove_file(&path).ok();

    // Err — the file cannot be parsed, so nothing can be merged into it.
    let path = tmp_config_path("persist-malformed");
    std::fs::write(&path, "{ not json").unwrap();
    let manager = build_manager_with_path(config_with(vec![account("acct-a", 0)]), path.clone());
    assert_eq!(manager.set_disabled(0, true), DisablePersist::WriteFailed);
    assert_eq!(manager.config.lock().unwrap().accounts[0].disabled, None);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "{ not json",
        "a malformed config must be left exactly as found"
    );
    std::fs::remove_file(&path).ok();
}

/// Every outcome the user could mistake for success must SAY so, and the two
/// that are fine must stay silent. This is the mapping the TUI renders, so a
/// silent failure arm here is a lie on screen.
#[test]
fn every_non_durable_persist_outcome_warns() {
    for disabled in [true, false] {
        for outcome in [DisablePersist::Persisted, DisablePersist::NoConfigFile] {
            assert_eq!(
                outcome.warning(disabled),
                None,
                "{outcome:?} is not a failure — warning about it would be noise"
            );
        }
        for outcome in [
            DisablePersist::NoSuchAccount,
            DisablePersist::NoEntry,
            DisablePersist::Ambiguous,
            DisablePersist::WriteFailed,
        ] {
            let warning = outcome
                .warning(disabled)
                .unwrap_or_else(|| panic!("{outcome:?} must warn — it did not persist"));
            assert!(
                warning.starts_with("NOT SAVED"),
                "{outcome:?} must lead with the headline so it survives truncation: {warning}"
            );
        }
    }
}

/// **THE direction guard.** Nothing was written, so the file still says what it
/// said — and that means the consequence of a failed write is the OPPOSITE in
/// the two directions. A failed `d` (disable) leaves the account in rotation; a
/// failed `e` (enable) leaves it BENCHED. One direction-blind line used to
/// claim "it returns to rotation on restart" for both, so a user whose enable
/// failed restarted on that advice and got the account benched anyway.
#[test]
fn a_failed_persist_states_the_consequence_of_its_own_direction() {
    for outcome in [DisablePersist::NoEntry, DisablePersist::WriteFailed] {
        let disabling = outcome.warning(true).expect("a failed disable must warn");
        assert!(
            disabling.ends_with("it returns to rotation on restart"),
            "a failed DISABLE leaves the account in rotation: {disabling}"
        );

        let enabling = outcome.warning(false).expect("a failed enable must warn");
        assert!(
            enabling.ends_with("it stays benched after a restart"),
            "a failed ENABLE leaves the account benched: {enabling}"
        );
        assert!(
            !enabling.contains("returns to rotation"),
            "a failed enable must never promise the opposite of what happens: {enabling}"
        );
    }
}

/// The ambiguity refusal must name an action that actually works. It used to
/// say "rename one to fix", which is provably wrong whenever both entries carry
/// an `accountUuid`: the match never consults names on that path, so renaming
/// changes nothing and the next `d` fails identically.
#[test]
fn the_ambiguity_warning_names_a_remedy_that_works() {
    let warning = DisablePersist::Ambiguous
        .warning(true)
        .expect("an ambiguous refusal must warn");
    assert!(
        warning.contains("orgUuid"),
        "the remedy is giving the entries distinct org keys: {warning}"
    );
    assert!(
        !warning.contains("rename"),
        "renaming does not break a UUID-matched tie: {warning}"
    );
}

/// A manager with no config file behind it reports that distinctly and does NOT
/// warn: `tcr demo` and `tcr status --probe` are memory-only by design, and a
/// "not saved" banner on every keypress there would be noise, not honesty.
#[test]
fn set_disabled_without_a_config_file_is_not_a_failure() {
    let manager = manager_of(vec![account("acct-a", 0)]);
    assert_eq!(manager.set_disabled(0, true), DisablePersist::NoConfigFile);
    assert_eq!(manager.set_disabled(9, true), DisablePersist::NoSuchAccount);
}

/// **INV2 / FIX 6.** The config file write must not run under the config mutex.
/// That mutex is taken on the PER-CONNECTION path — `state.rs`'s
/// `session_affinity_enabled()`, called from `mitm.rs`'s serve path — so holding
/// it across `save_disabled`'s read + `sync_all` + rename let one TUI keypress
/// stall connection setup on an fsync.
///
/// Measured by holding the config mutex here and requiring the write to complete
/// anyway. Under a persist that holds `config` across the save, the write cannot
/// start until this guard drops, so the poll below never sees the flag and the
/// test fails on its deadline.
#[test]
fn the_config_file_write_does_not_run_under_the_config_mutex() {
    let path = tmp_config_path("persist-inv2");
    write_account_file(&path, &["acct-a"]);
    let manager = build_manager_with_path(config_with(vec![account("acct-a", 0)]), path.clone());

    // Stand in for a request-path `session_affinity_enabled()` holding the lock.
    let held = manager.config.lock().expect("config lock poisoned");

    let writer = {
        let m = Arc::clone(&manager);
        std::thread::spawn(move || m.set_disabled(0, true))
    };

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        if read_config_json(&path)["accounts"][0]
            .get("disabled")
            .is_some()
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the config file write blocked on the config mutex — INV2 violated"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    // INV3 still applies: the memory update is what waits for this guard.
    drop(held);
    assert_eq!(
        writer.join().expect("persist thread panicked"),
        DisablePersist::Persisted
    );
    assert_eq!(
        manager.config.lock().unwrap().accounts[0].disabled,
        Some(true)
    );
    std::fs::remove_file(&path).ok();
}

/// **INV1 / FIX 6.** Concurrent persists must not interleave their
/// read-modify-write of the file. Each of these threads benches a DIFFERENT
/// account, so every flag must be present at the end; an interleaved
/// read-modify-write drops the flags written between another thread's read and
/// its rename, which shows up here as a missing flag.
#[test]
fn concurrent_persists_do_not_lose_each_others_writes() {
    const ACCOUNTS: usize = 8;
    let names: Vec<String> = (0..ACCOUNTS).map(|i| format!("acct-{i}")).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let path = tmp_config_path("persist-inv1");
    write_account_file(&path, &refs);
    let manager = build_manager_with_path(
        config_with(names.iter().map(|n| account(n, 0)).collect()),
        path.clone(),
    );

    let threads: Vec<_> = (0..ACCOUNTS)
        .map(|idx| {
            let m = Arc::clone(&manager);
            std::thread::spawn(move || m.set_disabled(idx, true))
        })
        .collect();
    for thread in threads {
        assert_eq!(
            thread.join().expect("persist thread panicked"),
            DisablePersist::Persisted
        );
    }

    let after = read_config_json(&path);
    for idx in 0..ACCOUNTS {
        assert_eq!(
            after["accounts"][idx]["disabled"],
            serde_json::json!(true),
            "account {idx}'s flag was lost to an interleaved write: {after}"
        );
    }
    assert_eq!(
        after["warmupSeconds"],
        serde_json::json!(900),
        "an unmodelled key was dropped by the concurrent writes"
    );
    std::fs::remove_file(&path).ok();
}

/// An out-of-range index is a no-op on both halves: nothing flips in memory
/// and nothing is written, rather than persisting a flag for an account that
/// does not exist.
#[test]
fn set_disabled_out_of_range_writes_nothing() {
    let path = tmp_config_path("durable-disable-oob");
    write_account_file(&path, &["acct-a"]);
    let before = std::fs::read_to_string(&path).unwrap();
    let manager = build_manager_with_path(config_with(vec![account("acct-a", 0)]), path.clone());

    manager.set_disabled(9, true);

    assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    std::fs::remove_file(&path).ok();
}
