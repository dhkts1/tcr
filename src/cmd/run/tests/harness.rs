//! Tests of how `tcr run` finds and checks what it launches: the harness binary, the withheld
//! proxy key, the `claude` version gate, `--group` validation and the see-through CA.

use std::path::{Path, PathBuf};

use tcr::verbs::cli::validate_group_label_chars;
use tcr::{base::config, base::tls};

use crate::cmd::run::launch_env::{LaunchKey, apply_nested_claude_ca, see_through_ca_notice};
use crate::cmd::run::{
    ClaudeVersionCheck, CodexLaunch, apply_codex_launch, classify_claude_version_output,
    parse_claude_version, resolve_harness_bin_from, see_through_ca_in, validate_group,
    withheld_api_key_notice,
};
use crate::cmd::server::default_config;
use crate::cmd::test_support::{account_with_groups, env_changes, inherited, test_host};

/// `TCR_CLAUDE_BIN` outranks `CLAUDE_BIN` when both are set.
#[test]
fn tcr_claude_bin_wins_over_the_legacy_key() {
    assert_eq!(
        resolve_harness_bin_from(
            Some("/opt/beta/claude".to_string()),
            Some("/opt/legacy/claude".to_string()),
            "claude",
        ),
        "/opt/beta/claude",
    );
}

/// `CLAUDE_BIN` is used when the tcr-specific key is unset.
#[test]
fn claude_bin_is_the_fallback_when_tcr_claude_bin_is_unset() {
    assert_eq!(
        resolve_harness_bin_from(None, Some("/opt/legacy/claude".to_string()), "claude"),
        "/opt/legacy/claude",
    );
}

/// An empty value counts as unset, for either key — a shell that leaves
/// `TCR_CLAUDE_BIN=` behind must not silently win over a real `CLAUDE_BIN`.
#[test]
fn an_empty_value_is_treated_as_missing() {
    assert_eq!(
        resolve_harness_bin_from(
            Some(String::new()),
            Some("/opt/legacy/claude".to_string()),
            "claude",
        ),
        "/opt/legacy/claude",
        "an empty TCR_CLAUDE_BIN must fall through to CLAUDE_BIN"
    );
    assert_eq!(
        resolve_harness_bin_from(Some(String::new()), Some(String::new()), "claude"),
        "claude",
        "two empty values must fall through to the default"
    );
}

/// Neither key set falls back to the default (`claude` on `PATH`).
#[test]
fn neither_key_set_falls_back_to_the_default() {
    assert_eq!(resolve_harness_bin_from(None, None, "claude"), "claude");
}

/// Pins that a configured proxy key is withheld from `claude`, and says why.
#[test]
fn the_proxy_key_is_withheld_from_claude_with_the_reason() {
    let notice = withheld_api_key_notice(true, false).expect("a configured key is withheld");
    assert!(
        notice.contains("connector"),
        "the notice must name what exporting it costs: {notice}"
    );
    assert!(
        notice.contains("Export it yourself"),
        "the notice must name the escape hatch: {notice}"
    );

    assert_eq!(
        withheld_api_key_notice(false, false),
        None,
        "nothing is withheld when no proxy key is configured"
    );
    assert_eq!(
        withheld_api_key_notice(true, true),
        None,
        "a key the caller exported is their choice — we neither replace it nor comment"
    );
}

// ---- `--group` claude version gate (`classify_claude_version_output`) -----

#[test]
fn claude_version_parses_the_real_output_shape() {
    assert_eq!(
        parse_claude_version("2.1.237 (Claude Code)"),
        Some((2, 1, 237))
    );
    assert_eq!(parse_claude_version("2.1.237"), Some((2, 1, 237)));
}

#[test]
fn claude_version_parse_fails_closed_on_garbage() {
    for garbage in ["", "not a version", "v2.1.237", "2.1", "2.1.abc"] {
        assert_eq!(
            parse_claude_version(garbage),
            None,
            "{garbage:?} must not parse as a version"
        );
    }
}

#[test]
fn claude_version_too_old_refuses() {
    assert_eq!(
        classify_claude_version_output("2.1.226 (Claude Code)"),
        ClaudeVersionCheck::TooOld("2.1.226".to_string())
    );
    assert_eq!(
        classify_claude_version_output("1.9.999 (Claude Code)"),
        ClaudeVersionCheck::TooOld("1.9.999".to_string())
    );
}

/// The comparison must be NUMERIC per-component, not lexicographic on the
/// version string. `"2.1.9"` sorts AFTER `"2.1.227"` as a plain string
/// (`'9' > '2'`), which would wrongly classify it `Ok` against the
/// 2.1.227 minimum; numerically `9 < 227`, so it must be `TooOld`. Every
/// other version-gate test here (`2.1.226`/`1.9.999` vs `2.1.227`) would
/// pass under EITHER comparison and so does not guard this property —
/// this is the one case where the two implementations disagree.
#[test]
fn claude_version_compares_patch_numerically_not_lexicographically() {
    assert_eq!(
        classify_claude_version_output("2.1.9 (Claude Code)"),
        ClaudeVersionCheck::TooOld("2.1.9".to_string()),
        "2.1.9 must be TooOld against a 2.1.227 minimum — a lexicographic \
         compare would wrongly say Ok because '9' > '2' as characters"
    );
}

#[test]
fn claude_version_at_or_above_minimum_is_ok() {
    assert_eq!(
        classify_claude_version_output("2.1.227 (Claude Code)"),
        ClaudeVersionCheck::Ok
    );
    assert_eq!(
        classify_claude_version_output("2.1.237 (Claude Code)"),
        ClaudeVersionCheck::Ok
    );
    assert_eq!(
        classify_claude_version_output("3.0.0 (Claude Code)"),
        ClaudeVersionCheck::Ok
    );
}

#[test]
fn claude_version_unparseable_output_warns_and_proceeds() {
    assert_eq!(
        classify_claude_version_output("garbage"),
        ClaudeVersionCheck::Unknown
    );
    assert_eq!(
        classify_claude_version_output(""),
        ClaudeVersionCheck::Unknown
    );
}

// ---- `--group` name validation (`validate_group`) --------------------------

#[test]
fn validate_group_accepts_a_configured_group() {
    let mut config = default_config();
    config.accounts = vec![
        account_with_groups("alice", Some(&["codereview"])),
        account_with_groups("bob", None),
    ];
    assert!(validate_group(&config, "codereview").is_ok());
}

#[test]
fn validate_group_rejects_an_unknown_name_and_lists_the_configured_groups() {
    let mut config = default_config();
    config.accounts = vec![
        account_with_groups("alice", Some(&["codereview", "burst"])),
        account_with_groups("bob", Some(&["burst"])),
    ];
    let err = validate_group(&config, "typo-group")
        .expect_err("an unconfigured group name must be refused");
    let msg = err.to_string();
    assert!(
        msg.contains("burst") && msg.contains("codereview"),
        "the error must name every configured group so the operator can fix the typo: {msg}"
    );
}

#[test]
fn validate_group_rejects_when_no_account_has_any_group() {
    let mut config = default_config();
    config.accounts = vec![account_with_groups("alice", None)];
    assert!(
        validate_group(&config, "codereview").is_err(),
        "a typo must never silently resolve to the empty set — that routes everywhere"
    );
}

// ---- `--group` label character validation (`validate_group_label_chars`) --

#[test]
fn group_label_chars_accepts_an_ordinary_ascii_label() {
    assert_eq!(validate_group_label_chars("codereview"), Ok(()));
    assert_eq!(validate_group_label_chars("code-review_2"), Ok(()));
}

#[test]
fn group_label_chars_rejects_empty_and_whitespace_only() {
    assert_eq!(
        validate_group_label_chars(""),
        Err("empty or whitespace-only")
    );
    assert_eq!(
        validate_group_label_chars("   "),
        Err("empty or whitespace-only")
    );
}

#[test]
fn group_label_chars_rejects_a_newline() {
    assert_eq!(
        validate_group_label_chars("code\nreview"),
        Err("contains a newline")
    );
    assert_eq!(
        validate_group_label_chars("code\rreview"),
        Err("contains a newline")
    );
}

#[test]
fn group_label_chars_rejects_other_control_characters() {
    assert_eq!(
        validate_group_label_chars("code\0review"),
        Err("contains a control character")
    );
    assert_eq!(
        validate_group_label_chars("code\treview"),
        Err("contains a control character")
    );
}

#[test]
fn group_label_chars_rejects_codepoints_above_u00ff() {
    assert_eq!(
        validate_group_label_chars("codereview\u{1F600}"), // an emoji
        Err("contains a codepoint above U+00FF")
    );
}

#[test]
fn validate_group_rejects_a_group_argument_with_an_embedded_newline() {
    let mut config = default_config();
    config.accounts = vec![account_with_groups("alice", Some(&["codereview"]))];
    let err = validate_group(&config, "code\nreview")
        .expect_err("a newline in the --group argument must be refused");
    assert!(
        err.to_string().contains("newline"),
        "the error must name the character class at fault: {err}"
    );
}

#[test]
fn validate_group_rejects_a_config_declared_label_with_a_control_character() {
    let mut config = default_config();
    config.accounts = vec![account_with_groups("alice", Some(&["code\0review"]))];
    let err = validate_group(&config, "codereview")
        .expect_err("a control character in a CONFIG-declared label must also be refused");
    assert!(
        err.to_string().contains("control character"),
        "the error must name the character class at fault: {err}"
    );
}

/// `tcr run --group` launches a CLAUDE session, and no Codex account can
/// serve one: a group whose every member is a Codex account is refused before
/// anything launches, while a group that also holds a Claude account launches
/// as before.
#[test]
fn run_group_with_no_claude_member_is_refused() {
    let mut config = default_config();
    config.accounts = vec![
        account_with_groups("alice", Some(&["review"])),
        config::Account {
            provider: tcr::base::provider::Provider::Codex,
            ..account_with_groups("bob-codex", Some(&["lab", "review"]))
        },
    ];
    let err = validate_group(&config, "lab").expect_err("lab holds no Claude account");
    assert_eq!(
        err.to_string(),
        "group 'lab' has no Claude account (its members are Codex accounts, which a Claude session cannot use) — nothing launched."
    );
    assert!(validate_group(&config, "review").is_ok());
}

/// `spillToPool` does not lift that refusal. With it, every request of such a
/// session would fall back to the whole pool, so the session would run and
/// never once be served by the group it asked for. The launch-time check
/// already treats a label no account carries the same way, spill or not, and
/// a group with no Claude account is that, to a Claude session.
#[test]
fn run_group_refuses_a_codex_only_group_even_when_it_spills_to_the_pool() {
    let mut config = default_config();
    config.accounts = vec![
        account_with_groups("alice", Some(&["review"])),
        config::Account {
            provider: tcr::base::provider::Provider::Codex,
            ..account_with_groups("bob-codex", Some(&["lab"]))
        },
    ];
    config.group_settings.insert(
        "lab".to_string(),
        config::GroupSettings {
            spill_to_pool: true,
            ..Default::default()
        },
    );
    assert!(config.spill_group_names().contains("lab") && !config.is_group_reserved("lab"));
    let err =
        validate_group(&config, "lab").expect_err("spilling does not give lab a Claude account");
    assert_eq!(
        err.to_string(),
        tcr::verbs::provider_guard::group_has_no_claude("lab")
    );
}

fn persisted_tls_files(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            let bytes = std::fs::read(&path).unwrap();
            (path, bytes)
        })
        .collect();
    files.sort_by(|left, right| left.0.cmp(&right.0));
    files
}

#[test]
fn see_through_ca_keeps_a_running_legacy_proxys_chain_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let ca = tls::load_tls_in(dir.path()).unwrap().ca_path;
    std::fs::remove_file(dir.path().join("tcr-ca.key")).unwrap();
    std::fs::write(
        dir.path().join("tcr-leaf.sans"),
        "api.anthropic.com\nplatform.claude.com\nwww.example.org\n",
    )
    .unwrap();
    let before = persisted_tls_files(dir.path());

    assert_eq!(see_through_ca_in(dir.path()), Some(ca));

    assert!(
        persisted_tls_files(dir.path()) == before,
        "launching a client must not rotate the running proxy's CA or leaf"
    );
    assert!(!dir.path().join("tcr-ca.key").exists());
}

#[test]
fn see_through_ca_does_not_create_certificate_material() {
    let dir = tempfile::tempdir().unwrap();
    let absent = dir.path().join("absent");
    assert_eq!(see_through_ca_in(&absent), None);
    assert!(!absent.exists(), "only server startup may create a chain");
}

#[test]
fn see_through_ca_reuses_a_fresh_servers_ca() {
    let dir = tempfile::tempdir().unwrap();
    let ca = tls::load_tls_in(dir.path()).unwrap().ca_path;
    let before = persisted_tls_files(dir.path());
    assert_eq!(see_through_ca_in(dir.path()), Some(ca));
    assert_eq!(persisted_tls_files(dir.path()), before);
}

/// Claude's trust notice names only the setting its own connection needs.
#[test]
fn the_claude_see_through_notice_names_only_its_ca() {
    let ca = Path::new("/Users/example/.config/tcr-ca.pem");
    assert_eq!(
        see_through_ca_notice(ca),
        "[tcr] trusting our MITM leaf via NODE_EXTRA_CA_CERTS=/Users/example/.config/tcr-ca.pem"
    );
}

/// A pooled codex session carries `HTTPS_PROXY` aimed at tcr, so a `claude` that codex
/// starts meets tcr's leaf for `api.anthropic.com`, and node trusts it only through
/// `NODE_EXTRA_CA_CERTS`. A pooled launch sets it to tcr's CA, unless the caller exported one,
/// which the child then inherits untouched. A direct launch meets no leaf and sets nothing.
#[test]
fn a_claude_inside_a_pooled_codex_gets_node_extra_ca_certs() {
    let ca = Path::new("/tmp/ca.pem");
    let mut cmd = std::process::Command::new("true");
    apply_nested_claude_ca(&mut cmd, ca, false);
    assert_eq!(
        env_changes(&cmd).get("NODE_EXTRA_CA_CERTS"),
        Some(&Some("/tmp/ca.pem".to_string()))
    );
    let mut cmd = std::process::Command::new("true");
    apply_nested_claude_ca(&mut cmd, ca, true);
    assert_eq!(
        env_changes(&cmd).get("NODE_EXTRA_CA_CERTS"),
        None,
        "the caller's own value is inherited, not replaced"
    );

    for (caller, expected) in [
        (inherited(&[]), Some(Some("/tmp/ca.pem".to_string()))),
        (
            inherited(&[("NODE_EXTRA_CA_CERTS", "/tmp/corporate-ca.pem")]),
            None,
        ),
    ] {
        let mut cmd = std::process::Command::new("true");
        let _lines = apply_codex_launch(
            &mut cmd,
            3999,
            &CodexLaunch::SeeThrough {
                ca: ca.to_path_buf(),
                no_proxy: Vec::new(),
            },
            &LaunchKey::NoKeyInConfig,
            &test_host(&caller),
        );
        assert_eq!(
            env_changes(&cmd).get("NODE_EXTRA_CA_CERTS").cloned(),
            expected,
            "a pooled launch sets the CA for a nested claude unless the caller exported one"
        );
    }

    let mut cmd = std::process::Command::new("true");
    let _lines = apply_codex_launch(
        &mut cmd,
        3999,
        &CodexLaunch::Direct {
            reason: "no proxy answered on 127.0.0.1:3999".to_string(),
            unset: Vec::new(),
        },
        &LaunchKey::NoKeyInConfig,
        &test_host(&inherited(&[])),
    );
    assert_eq!(env_changes(&cmd).get("NODE_EXTRA_CA_CERTS"), None);
}
