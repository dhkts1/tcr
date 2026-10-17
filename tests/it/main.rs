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

/// Shared by `delete_account_live` and `first_run_no_config`.
#[path = "../support/connect_guard.rs"]
mod connect_guard;

/// The environment every launch of the built `tcr` starts from; used by most modules below.
#[path = "../support/tcr_env.rs"]
mod tcr_env;

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
#[path = "../import.rs"]
mod import;
#[path = "../legacy_config_shapes.rs"]
mod legacy_config_shapes;
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

/// The first run of ten or more spaces that sits INSIDE a string literal on `line`, with a
/// word on each side. That is what a `\` line continuation leaves when something later joins the
/// pieces and keeps the next line's indentation: a log line that reads "... carry is      not on".
/// Indentation (the run before the first non-space) is not inside a literal, and the quote count
/// tells the two apart. `\"` is skipped; a raw string is not understood, which only matters for
/// a line this heuristic would then miss.
fn space_run_inside_literal(line: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut in_literal = false;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if in_literal => i += 1,
            b'"' => in_literal = !in_literal,
            b' ' if in_literal => {
                let run = bytes[i..].iter().take_while(|b| **b == b' ').count();
                let word_before = i > 0 && bytes[i - 1] != b'"';
                let word_after = bytes.get(i + run).is_some_and(|b| *b != b'"');
                if run >= 10 && word_before && word_after {
                    return Some(run);
                }
                i += run - 1;
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Files whose literals are column-aligned output on purpose: the tables `tcr stats` prints, the
/// `config show` columns and a pretty-printed JSON body under test.
const ALIGNED_ON_PURPOSE: [&str; 3] = ["wrap.rs", "config_show.rs", "cache_ttl.rs"];

/// Positive control: the scanner must flag the shape it exists for, and pass the shapes it must not.
#[test]
fn the_space_run_scanner_flags_a_continuation_scar() {
    let scar = r#"        "the lock is poisoned, so this carry is                          not on any hour""#;
    assert_eq!(space_run_inside_literal(scar), Some(26));
    assert_eq!(
        space_run_inside_literal(r#"        "one sentence, single spaces""#),
        None
    );
    assert_eq!(
        space_run_inside_literal("                let x = 1; // a            b"),
        None
    );
    assert_eq!(
        space_run_inside_literal(r#"    "  claude-opus-5                requests=2""#),
        Some(16),
        "an aligned table cell is a run too: such a file is exempt by name, not by the scanner"
    );
}

/// **No log or error message in `src/` carries a run of spaces from a joined line continuation.**
/// `tracing::warn!("a \<newline>   b")` drops the newline AND the indentation, but a rewrite that
/// joined the pieces onto one line kept the indentation inside the text, and the log read as
/// "so this carry is                          not on any path's hour" (found in
/// `peer/listener.rs`, `reach.rs`, `serve.rs`, `tui.rs`, `discovery.rs`).
#[test]
fn no_string_literal_in_src_carries_a_run_of_spaces() {
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read a src dir") {
            let path = entry.expect("a src entry").path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    walk(&root, &mut files);
    assert!(
        files.iter().any(|path| path.ends_with("peer/listener.rs")),
        "the walk must reach src/peer/listener.rs, or it checks nothing: {files:?}"
    );
    let mut scars = Vec::new();
    for path in files {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
        if name.is_some_and(|n| ALIGNED_ON_PURPOSE.contains(&n.as_str())) {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("read a src file");
        for (number, line) in text.lines().enumerate() {
            if let Some(run) = space_run_inside_literal(line) {
                scars.push(format!("{}:{}: {run} spaces", path.display(), number + 1));
            }
        }
    }
    assert!(
        scars.is_empty(),
        "a string literal holds a run of spaces (a joined line continuation?):\n{}",
        scars.join("\n")
    );
}
