//! Tests of `tcr server`: the stand-down exit codes, the migration write gate, config loading
//! and the shutdown lines.

use super::*;
use build_info::StandDownBuild;
use tcr::base::log_err::LogErr as _;

fn silent() -> live_client::Liveness {
    live_client::Liveness::Silent {
        why: "the server did not answer within 5s".to_string(),
    }
}

/// A WEDGED INCUMBENT MUST NOT BE OFFERED A RECOVERY THAT CANNOT WORK.
///
/// `--replace` is refused for an embedded incumbent on every path in
/// `singleton::takeover_decision`, deliberately: the pid is the host
/// application's. Printing "Run `tcr --replace`" for that kind sends the
/// operator around a loop that ends in the same stand-down, while the
/// instruction that does work — quit the host application — never appears.
#[test]
fn a_wedged_embedded_incumbent_is_not_told_to_run_replace() {
    let embedded = wedged_incumbent_recovery(singleton::ProxyKind::TcrEmbedded);
    assert!(
        !embedded.contains("Run `tcr --replace`"),
        "must not prescribe an override that this kind refuses: {embedded}"
    );
    assert!(
        embedded.contains("Quit the host application"),
        "must name the recovery that works: {embedded}"
    );
    // The control: for the kind `--replace` CAN take over, the prescription is
    // unchanged — so the assertions above are about the kind, not about the
    // advice having been dropped for everyone.
    let advice = wedged_incumbent_recovery(singleton::ProxyKind::Tcr);
    assert!(
        advice.contains("Stop the proxy explicitly")
            && advice.contains(&format!("tcr {} or newer", build_info::HANDOFF_SINCE)),
        "standalone recovery must explain when handoff is available: {advice}"
    );
}

/// The ordinary stand-down: a peer is serving this binary's commit. Exit 0,
/// or every `tcr` in a script becomes a failure.
#[test]
fn a_clean_stand_down_exits_zero() {
    for verdict in [
        StandDownBuild::InSync,
        StandDownBuild::DirtyBuild,
        StandDownBuild::Unknown,
    ] {
        assert_eq!(
            stand_down_exit_code(&live_client::Liveness::Answering, verdict),
            0,
            "{verdict:?} is a working incumbent"
        );
    }
}

/// DETECTED BUILD SKEW MUST NOT EXIT 0. The whole point of the stale-server
/// warning is that `cargo build && tcr` no longer guarantees the new binary
/// is serving; returning success anyway leaves the guarantee broken for every
/// script, CI step and launchd job, which read the code and not the stderr.
#[test]
fn a_stale_incumbent_exits_non_zero() {
    let code = stand_down_exit_code(&live_client::Liveness::Answering, StandDownBuild::Stale);
    assert_ne!(code, 0, "a detected skew must be visible to `tcr && next`");
    assert_ne!(code, 1, "1 is a genuine startup failure");
    assert_ne!(code, 2, "2 is clap's usage error");
    assert_eq!(code, EXIT_STOOD_DOWN_STALE);
}

/// THE WEDGED PROXY. Nothing is serving on the port, so exiting 0 tells every
/// caller — and TcrBar — that the server is up. The code has to say
/// otherwise, and it outranks the build verdict: a proxy that will not answer
/// cannot report a build either, so `Unknown` is what it always comes with.
#[test]
fn a_silent_incumbent_exits_its_own_non_zero_code() {
    assert_eq!(
        stand_down_exit_code(&silent(), StandDownBuild::Unknown),
        EXIT_STOOD_DOWN_NOT_ANSWERING
    );
    assert_ne!(EXIT_STOOD_DOWN_NOT_ANSWERING, 0);
    assert_ne!(
        EXIT_STOOD_DOWN_NOT_ANSWERING, EXIT_STOOD_DOWN_STALE,
        "the two need different recoveries, so they need different codes"
    );
    // Liveness outranks every build verdict, including a comparable one.
    assert_eq!(
        stand_down_exit_code(&silent(), StandDownBuild::InSync),
        EXIT_STOOD_DOWN_NOT_ANSWERING,
        "a matching sha from a process that answers nothing is not a healthy port"
    );
}

/// THE EXIT CODES ARE A CROSS-LANGUAGE CONTRACT, exactly like
/// `singleton::INCUMBENT_MARKER`, and nothing but this test couples them.
///
/// TcrBar switches on the numbers: `ServerController.StandDownExit` in
/// `apps/macos/Sources/TcrBarCore/ServerController.swift` declares
/// `stale = 3` and `notAnswering = 4`, and `classifyExit` turns them into
/// `.incumbentIsStale` and `.incumbentNotAnswering`. Renumbering a constant
/// here is a one-character edit that every other Rust test survives, while
/// the menu-bar app silently falls through to a bare `.exited(5, …)` and
/// reports a wedged proxy — one serving NOTHING — as a clean exit. That is
/// the misreport this whole round exists to eliminate.
///
/// The numbers are SPELLED OUT rather than referenced through the constants,
/// deliberately: `assert_eq!(EXIT_STOOD_DOWN_STALE, EXIT_STOOD_DOWN_STALE)`
/// compares a value with itself and passes for every value of it. The
/// constant is the thing that must not drift, so the test has to hold the
/// other copy — the one Swift carries.
#[test]
fn the_stand_down_exit_codes_are_the_numbers_tcrbar_switches_on() {
    // Transcribed from ServerController.StandDownExit.
    let tcrbar_stale: i32 = 3;
    let tcrbar_not_answering: i32 = 4;

    assert_eq!(
        EXIT_STOOD_DOWN_OK, 0,
        "a clean stand-down is success; anything else fails every `tcr && next`"
    );
    assert_eq!(
        EXIT_STOOD_DOWN_STALE, tcrbar_stale,
        "ServerController.StandDownExit.stale is 3 — change one, change both"
    );
    assert_eq!(
        EXIT_STOOD_DOWN_NOT_ANSWERING, tcrbar_not_answering,
        "ServerController.StandDownExit.notAnswering is 4 — change one, change both"
    );

    // The constants being right is worthless if the mapping does not emit
    // them, so the contract is asserted through the function TcrBar's input
    // actually comes from, against the same literals.
    assert_eq!(
        stand_down_exit_code(&live_client::Liveness::Answering, StandDownBuild::InSync),
        0
    );
    assert_eq!(
        stand_down_exit_code(&live_client::Liveness::Answering, StandDownBuild::Stale),
        3,
        "a stale incumbent must reach Swift as .incumbentIsStale"
    );
    assert_eq!(
        stand_down_exit_code(&silent(), StandDownBuild::Unknown),
        4,
        "a wedged incumbent must reach Swift as .incumbentNotAnswering"
    );

    // Liveness outranks build skew across the boundary too: a proxy that
    // answers nothing is not merely stale, and 4 must win over 3 — the
    // operator's next command differs (recover, not upgrade).
    assert_eq!(
        stand_down_exit_code(&silent(), StandDownBuild::Stale),
        4,
        "NOT SERVING outranks a stale build; reporting 3 here understates it"
    );

    // Three outcomes, three codes. Two of them collapsing would make the
    // Swift switch pick one arm for both.
    let codes = [
        EXIT_STOOD_DOWN_OK,
        EXIT_STOOD_DOWN_STALE,
        EXIT_STOOD_DOWN_NOT_ANSWERING,
    ];
    for (i, a) in codes.iter().enumerate() {
        for b in &codes[i + 1..] {
            assert_ne!(a, b, "the stand-down codes must stay distinct: {codes:?}");
        }
    }
    // And neither may take a code that already means something else.
    assert!(
        !codes[1..].contains(&1),
        "1 is a genuine startup failure (anyhow::Error out of main)"
    );
    assert!(
        !codes[1..].contains(&2),
        "2 is clap's usage error, which TcrBar maps to the unknown-argument hint"
    );
}

// --- legacy `throttle` migration: boot-time behaviour ---------------------

fn unique_config_path(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("tcr-main-{tag}-{}-{seq}.json", std::process::id()))
}

/// `migration_persist_target` gates the write `run_server` performs after a
/// migration: an active migration with nowhere quarantined and a real path
/// on disk gets rewritten.
#[test]
fn migration_persist_target_writes_when_clear() {
    let path = unique_config_path("persist-target-clear");
    let mut config = default_config();
    config.migrated_legacy_throttle = true;
    assert_eq!(
        migration_persist_target(&config, &Some(path.clone())),
        Some(path.as_path())
    );
}

/// The quarantine gate is not optional (mirrors `cli::load_for_edit`):
/// writing back a `Config` while an account is quarantined would serialize
/// over that account's raw JSON (its `importFrom` pointer included) and
/// drop it permanently. A pending migration must stay in-memory-only until
/// a human clears the quarantine.
#[test]
fn migration_persist_target_is_none_when_an_account_is_quarantined() {
    let path = unique_config_path("persist-target-quarantined");
    let mut config = default_config();
    config.migrated_legacy_throttle = true;
    config.quarantined_accounts = vec!["acct-import".to_string()];
    assert_eq!(migration_persist_target(&config, &Some(path)), None);
}

/// Nothing to persist when `load` never migrated anything.
#[test]
fn migration_persist_target_is_none_when_nothing_migrated() {
    let path = unique_config_path("persist-target-unmigrated");
    let config = default_config();
    assert_eq!(migration_persist_target(&config, &Some(path)), None);
}

/// Nothing to persist without a file path (e.g. the corrupt-config fallback
/// used to drop the persist path — see `load_config`).
#[test]
fn migration_persist_target_is_none_without_a_persist_path() {
    let mut config = default_config();
    config.migrated_legacy_throttle = true;
    assert_eq!(migration_persist_target(&config, &None), None);
}

/// A missing config file is a legitimate first run: `load_config` must
/// still boot on in-memory defaults, keeping the persist path so the first
/// refresh creates the file.
#[test]
fn load_config_creates_the_file_when_it_is_missing() {
    let path = unique_config_path("missing");
    let (config, persist_path) =
        load_config(&path).expect("a missing config file must not refuse to boot");
    assert!(config.accounts.is_empty());
    assert_eq!(persist_path, Some(path.clone()));
    // The server's first boot now LEAVES the file behind, so the `tcr
    // status` a user runs next finds the same config the server booted from
    // rather than nothing at all (`config::load_or_init`).
    assert!(
        path.exists(),
        "booting with no config must create {}",
        path.display()
    );
    std::fs::remove_file(&path).log_debug("removing a test config");
}

/// The behaviour this task changed: a config that exists and fails to
/// parse (malformed JSON, NOT the legacy `throttle` key — that key is
/// migrated, never an error, per `config::load`) must make the server
/// REFUSE to boot rather than silently serve a zero-account fleet that
/// answers every request with 429 while looking alive.
#[test]
fn load_config_refuses_to_boot_on_a_corrupt_config() {
    let path = unique_config_path("corrupt");
    std::fs::write(&path, "{ this is not valid json").unwrap();
    let err = load_config(&path).expect_err("a corrupt config must refuse to boot");
    assert!(
        err.to_string().contains("unreadable/corrupt"),
        "the refusal must say why: {err}"
    );
    std::fs::remove_file(&path).ok();
}

/// Every shutdown trigger must log a distinct, non-empty line.
///
/// `ServingStopped` previously logged NOTHING: its arm was `=> {}` and the
/// only statement after the match warns solely when tasks were aborted. A
/// process that stopped down that path left no trace of having stopped.
///
/// Distinctness matters as much as presence. `tests/proxy/headless_sigterm.rs`
/// asserts on the SIGTERM text specifically, so two triggers sharing a line
/// would make that assertion pass for the wrong trigger, which is a green
/// test for a broken reason.
#[test]
fn every_shutdown_trigger_logs_a_line() {
    let triggers = [
        ShutdownTrigger::CtrlC,
        ShutdownTrigger::Sigterm,
        ShutdownTrigger::ServingStopped,
        ShutdownTrigger::Replaced,
    ];
    let mut seen: Vec<&str> = Vec::new();
    for trigger in triggers {
        let line = trigger.shutdown_line();
        assert!(
            !line.trim().is_empty(),
            "{trigger:?} logs nothing on the way out"
        );
        assert!(
            !seen.contains(&line),
            "{trigger:?} shares its line with another trigger ({line:?}); \
             the SIGTERM assertion in tests/proxy/headless_sigterm.rs would then \
             pass for the wrong reason"
        );
        seen.push(line);
    }
    assert_eq!(
        ShutdownTrigger::Sigterm.shutdown_line(),
        "SIGTERM received; shutting down",
        "tests/proxy/headless_sigterm.rs matches this exact text; changing it here \
         without changing it there turns that test red for no real reason"
    );
}
