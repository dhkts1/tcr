//! The integration tests that can share one process, compiled and linked as one crate.
//!
//! Each `tests/<name>.rs` is a module here under its own name, so a test that was
//! `cargo test --test peer_wire <test>` is now `cargo test --test it peer_wire::<test>`, and
//! `cargo test --test it peer_wire::` runs the whole file. The files stay where they were, because
//! code and scripts across the tree cite them by path.
//!
//! A file that owns process-wide state (a global `tracing` subscriber, the peer-lease fallback,
//! the egress peers path, the router-mapping register, the loaded mitm leaf, or an in-process
//! `server::serve`, which installs several of those) is its own `[[test]]` in `Cargo.toml`, with
//! the reason beside it. A file added here must not install any of those: a sibling module in this
//! process would see it, and its tests run at the same time as yours.

#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fail the test with a message; the ban is for production code"
)]

/// Shared by `delete_account_live`, `first_run_no_config` and `migrate_names`.
#[path = "../support/connect_guard.rs"]
mod connect_guard;

/// The environment every launch of the built `tcr` starts from; used by most modules below.
#[path = "../support/tcr_env.rs"]
mod tcr_env;

/// The old names for `tcr::test_support`'s fixtures, still called by `peer_props` (and compiled
/// into the separate `peer_egress` and `peer_lease` too).
#[path = "../support/fixtures.rs"]
mod fixtures;

/// The harness pieces the `peer_*` modules share (and compiled into the separate `peer_*`
/// targets that use them too).
#[path = "../support/peer.rs"]
mod peer_support;

/// Shared by `codex_e2e` and `peer_e2e` (and compiled into the separate `peer_lease` too).
#[path = "../tools/api_contract.rs"]
mod api_contract;

#[path = "../cache_ttl_rewrite.rs"]
mod cache_ttl_rewrite;
#[path = "../claude_code_import.rs"]
mod claude_code_import;
#[path = "../codex_cli_login.rs"]
mod codex_cli_login;
#[path = "../codex_e2e.rs"]
mod codex_e2e;
#[path = "../codex_import.rs"]
mod codex_import;
#[path = "../codex_run.rs"]
mod codex_run;
#[path = "../config_show.rs"]
mod config_show;
#[path = "../delete_account_live.rs"]
mod delete_account_live;
#[path = "../doctor.rs"]
mod doctor;
#[path = "../first_run_no_config.rs"]
mod first_run_no_config;
#[path = "../headless_log_file.rs"]
mod headless_log_file;
#[path = "../headless_sigterm.rs"]
mod headless_sigterm;
#[path = "../legacy_config_shapes.rs"]
mod legacy_config_shapes;
#[path = "../legacy_install_boot.rs"]
mod legacy_install_boot;
#[path = "../migrate_names.rs"]
mod migrate_names;
#[path = "../peer_abuse.rs"]
mod peer_abuse;
#[path = "../peer_aim.rs"]
mod peer_aim;
#[path = "../peer_ask.rs"]
mod peer_ask;
#[path = "../peer_carried_control.rs"]
mod peer_carried_control;
#[path = "../peer_dead_drop_dial.rs"]
mod peer_dead_drop_dial;
#[path = "../peer_discovery.rs"]
mod peer_discovery;
#[path = "../peer_drop.rs"]
mod peer_drop;
#[path = "../peer_e2e.rs"]
mod peer_e2e;
#[path = "../peer_graph.rs"]
mod peer_graph;
#[path = "../peer_hand.rs"]
mod peer_hand;
#[path = "../peer_moved.rs"]
mod peer_moved;
#[path = "../peer_noise.rs"]
mod peer_noise;
#[path = "../peer_pairing.rs"]
mod peer_pairing;
#[path = "../peer_path_accounting.rs"]
mod peer_path_accounting;
#[path = "../peer_probe.rs"]
mod peer_probe;
#[path = "../peer_props.rs"]
mod peer_props;
#[path = "../peer_punch.rs"]
mod peer_punch;
#[path = "../peer_status.rs"]
mod peer_status;
#[path = "../peer_wire.rs"]
mod peer_wire;
#[path = "../peer_wire_saved.rs"]
mod peer_wire_saved;
#[path = "../setup_route.rs"]
mod setup_route;
#[path = "../status_check.rs"]
mod status_check;
#[path = "../tcr_run_alias.rs"]
mod tcr_run_alias;
#[path = "../throttle_exempt.rs"]
mod throttle_exempt;
#[path = "../workload_scenarios.rs"]
mod workload_scenarios;

/// **Every `tests/*.rs` is compiled exactly once, as a module here or as its own `[[test]]`.**
///
/// `Cargo.toml` sets `autotests = false`, so cargo no longer finds a test file on its own: a
/// file nobody listed builds nothing, runs nothing, and leaves the suite green. This reads the
/// directory (untracked files included, which is what a fresh checkout of a branch would carry)
/// and the two lists, and fails on a file in neither or in both.
#[test]
fn every_test_file_is_a_module_here_or_its_own_test_target() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let modules =
        std::fs::read_to_string(root.join("tests/it/main.rs")).expect("read tests/it/main.rs");
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).expect("read Cargo.toml");
    let mut files: Vec<String> = std::fs::read_dir(root.join("tests"))
        .expect("read tests/")
        .map(|entry| {
            entry
                .expect("a tests/ entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|name| name.ends_with(".rs"))
        .collect();
    files.sort();
    assert!(
        files.iter().any(|name| name == "peer_wire.rs")
            && files.iter().any(|name| name == "peer_lease.rs"),
        "the listing must see a module file and a separate target, or it checks nothing: {files:?}"
    );
    let wrong: Vec<String> = files
        .iter()
        .filter_map(|name| {
            let here = modules.contains(&format!("#[path = \"../{name}\"]"));
            let own = manifest.contains(&format!("path = \"tests/{name}\""));
            match (here, own) {
                (true, false) | (false, true) => None,
                (false, false) => Some(format!("{name}: in neither, so it never builds")),
                (true, true) => Some(format!("{name}: in both, so it builds and runs twice")),
            }
        })
        .collect();
    assert!(
        wrong.is_empty(),
        "add each file to tests/it/main.rs, or give it a [[test]] in Cargo.toml with the reason it \
         needs its own process:\n{}",
        wrong.join("\n")
    );
}
