//! Tests of the environment `tcr run` launches into: the see-through and base-URL routes, the
//! request key, the shell policy, the cmux marker and the `--group` header.

use std::path::{Path, PathBuf};

use tcr::{cli, status};

use crate::cmd::run::launch_env::{
    LaunchKey, LaunchRequestKey, apply_base_url_env, apply_codex_see_through_env,
    apply_see_through_env, compose_group_header,
};
use crate::cmd::run::{
    CodexLaunch, RUN_ACTIVE_ENV, apply_codex_launch, claude_command, mark_run_active,
};
use crate::cmd::test_support::{env_changes, inherited, test_host};

/// A nested Codex keeps its own login through the unmarked proxy, and keeps
/// whichever CA bundle it inherited rather than replacing it with tcr's.
#[test]
fn claude_see_through_env_preserves_a_nested_codex_ca_choice() {
    let mut cmd = std::process::Command::new("true");
    let _lines = apply_see_through_env(
        &mut cmd,
        3999,
        Path::new("/tmp/ca.pem"),
        &LaunchKey::NoKeyInConfig,
    );
    let set = |name: &str| {
        cmd.get_envs()
            .find_map(|(k, v)| (k == name).then(|| v.map(|v| v.to_string_lossy().into_owned())))
    };
    assert_eq!(
        set("CODEX_CA_CERTIFICATE"),
        None,
        "an unmarked nested codex never meets our leaf and keeps its own CA choice"
    );
    assert_eq!(
        set("NODE_EXTRA_CA_CERTS"),
        Some(Some("/tmp/ca.pem".to_string())),
        "the same CA claude trusts"
    );
    assert_eq!(
        set("HTTPS_PROXY"),
        Some(Some("http://127.0.0.1:3999".to_string())),
        "the proxy a nested codex inherits"
    );
}

/// Neither routing mode may hand `claude` an `ANTHROPIC_API_KEY`.
#[test]
fn no_routing_mode_gives_claude_an_api_key() {
    for (label, apply) in [
        (
            "see-through",
            &(|cmd: &mut std::process::Command| {
                let _lines = apply_see_through_env(cmd, 3456, Path::new("/tmp/ca.pem"), &sent(KEY));
            }) as &dyn Fn(&mut std::process::Command),
        ),
        (
            "base-URL",
            &(|cmd: &mut std::process::Command| {
                let _lines = apply_base_url_env(cmd, 3456, &sent(KEY));
            }),
        ),
    ] {
        let mut cmd = claude_command();
        apply(&mut cmd);
        assert!(
            !cmd.get_envs()
                .any(|(k, v)| k == "ANTHROPIC_API_KEY" && v.is_some()),
            "{label} mode must not set ANTHROPIC_API_KEY — it outranks the claude.ai \
             login and disables every claude.ai connector"
        );
    }
}

/// `codex`'s see-through env: both proxy spellings, because a client that reads only the
/// one we skipped would go direct and bypass the pool in silence, and the CA in the one
/// variable Codex reads it from. Nothing of Claude's: `NODE_EXTRA_CA_CERTS` means nothing to
/// `codex`, and `ANTHROPIC_BASE_URL` has no business in its environment.
#[test]
fn codex_see_through_env_sets_proxy_and_ca_and_nothing_anthropic() {
    let mut cmd = std::process::Command::new("true");
    let _lines = apply_codex_see_through_env(
        &mut cmd,
        3999,
        Path::new("/tmp/ca.pem"),
        false,
        &LaunchKey::ServerPredatesKeys,
    );
    let env: std::collections::HashMap<_, _> = cmd
        .get_envs()
        .filter_map(|(k, v)| {
            v.map(|v| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
        })
        .collect();
    assert_eq!(
        env.get("HTTPS_PROXY").map(String::as_str),
        Some("http://tcr-codex@127.0.0.1:3999")
    );
    assert_eq!(
        env.get("https_proxy").map(String::as_str),
        Some("http://tcr-codex@127.0.0.1:3999")
    );
    assert_eq!(
        env.get("CODEX_CA_CERTIFICATE").map(String::as_str),
        Some("/tmp/ca.pem")
    );
    assert!(!env.contains_key("ANTHROPIC_BASE_URL"));
    assert!(!env.contains_key("NODE_EXTRA_CA_CERTS"));
}

#[test]
fn codex_see_through_env_respects_the_callers_ca() {
    let mut cmd = std::process::Command::new("true");
    cmd.env("CODEX_CA_CERTIFICATE", "/tmp/corporate-ca.pem");
    let _lines = apply_codex_see_through_env(
        &mut cmd,
        3999,
        Path::new("/tmp/tcr-ca.pem"),
        true,
        &LaunchKey::NoKeyInConfig,
    );
    assert_eq!(
        cmd.get_envs()
            .find_map(|(key, value)| { (key == "CODEX_CA_CERTIFICATE").then_some(value.unwrap()) }),
        Some(std::ffi::OsStr::new("/tmp/corporate-ca.pem")),
        "an explicit caller CA must not be replaced"
    );
}

/// A fake request key, hex like the ones the server writes.
const KEY: &str = "0000111122223333444455556666777788889999aaaabbbbccccddddeeeeffff";

/// The launch decision that sends `key`.
fn sent(key: &str) -> LaunchKey {
    LaunchKey::Send(LaunchRequestKey::parse(key).expect("a URL-safe key"))
}

/// A running server's status read, parsed from the wire, with `requestKeyMode`
/// set to `mode` or left out, as a server older than request keys leaves it.
fn status_reporting(mode: Option<&str>) -> Result<status::StatusPayload, cli::LiveStatusError> {
    let mut wire = serde_json::json!({
        "kind": status::STATUS_KIND,
        "accounts": [],
        "build": {"sha": "abc1234"},
    });
    if let Some(mode) = mode {
        wire["requestKeyMode"] = mode.into();
    }
    Ok(serde_json::from_value(wire).expect("a status payload"))
}

/// Every decision that launches unkeyed, one of each.
fn unkeyed_decisions() -> [LaunchKey; 4] {
    [
        LaunchKey::NoKeyInConfig,
        LaunchKey::KeyNotUrlSafe,
        LaunchKey::StatusUnread("nothing is listening".to_string()),
        LaunchKey::ServerPredatesKeys,
    ]
}

/// The gate: the key goes only to a server whose status carries
/// `requestKeyMode`, whatever its value, and only when the config holds a key a
/// URL can carry. Every other case is unkeyed with one notice that says why and
/// never carries the key.
#[test]
fn the_request_key_is_sent_only_to_a_server_that_reports_request_key_mode() {
    let log = status_reporting(Some("log"));
    assert_eq!(LaunchKey::decide(Some(KEY), &log), sent(KEY));
    assert_eq!(
        LaunchKey::decide(Some(KEY), &status_reporting(Some("enforce"))),
        sent(KEY),
        "presence is the contract, not the value"
    );
    assert_eq!(
        LaunchKey::decide(Some(KEY), &status_reporting(None)),
        LaunchKey::ServerPredatesKeys,
        "an older server compares the whole Codex header: a keyed URL would run unpooled"
    );
    assert_eq!(
        LaunchKey::decide(Some(KEY), &Err(cli::LiveStatusError::NoServer)),
        LaunchKey::StatusUnread("nothing is listening".to_string())
    );
    assert_eq!(
        LaunchKey::decide(
            Some(KEY),
            &Err(cli::LiveStatusError::Unusable("HTTP 401".to_string()))
        ),
        LaunchKey::StatusUnread("HTTP 401".to_string())
    );
    assert_eq!(LaunchKey::decide(None, &log), LaunchKey::NoKeyInConfig);
    assert_eq!(LaunchKey::decide(Some(""), &log), LaunchKey::NoKeyInConfig);
    assert_eq!(
        LaunchKey::decide(Some("not a:url@key"), &log),
        LaunchKey::KeyNotUrlSafe
    );

    assert_eq!(sent(KEY).key().map(LaunchRequestKey::as_str), Some(KEY));
    assert_eq!(
        sent(KEY).notice(3999),
        None,
        "a keyed launch says nothing more"
    );
    for decision in unkeyed_decisions() {
        assert_eq!(decision.key(), None, "{decision:?}");
        let notice = decision
            .notice(3999)
            .unwrap_or_else(|| panic!("{decision:?} launches unkeyed and must say why"));
        assert!(
            notice.starts_with("[tcr] launching without the request key: "),
            "{notice}"
        );
    }
    assert!(
        !format!("{:?}", sent(KEY)).contains(KEY),
        "Debug must not print the key"
    );
}

/// (a): keyed, both proxy spellings carry the key as the plain `tcr` user's
/// password; unkeyed, exactly the URL `tcr run` set before keys.
#[test]
fn a_keyed_see_through_launch_puts_the_key_in_both_proxy_urls() {
    let keyed = format!("http://tcr:{KEY}@127.0.0.1:3999");
    let mut cmd = std::process::Command::new("true");
    let _lines = apply_see_through_env(&mut cmd, 3999, Path::new("/tmp/ca.pem"), &sent(KEY));
    let env = env_changes(&cmd);
    assert_eq!(env.get("HTTPS_PROXY"), Some(&Some(keyed.clone())));
    assert_eq!(env.get("https_proxy"), Some(&Some(keyed)));

    for decision in unkeyed_decisions() {
        let mut cmd = std::process::Command::new("true");
        let _lines = apply_see_through_env(&mut cmd, 3999, Path::new("/tmp/ca.pem"), &decision);
        let env = env_changes(&cmd);
        for var in ["HTTPS_PROXY", "https_proxy"] {
            assert_eq!(
                env.get(var),
                Some(&Some("http://127.0.0.1:3999".to_string())),
                "{decision:?}: {var}"
            );
        }
    }
}

/// The `Proxy-Authorization` a client builds from `url` for an https request,
/// by the matcher codex's HTTP stack runs, read back by the server's own parse.
fn server_reads(url: &str) -> tcr::request_key::ProxyCredential {
    use hyper_util::client::proxy::matcher::Matcher;
    let target: hyper::Uri = "https://chatgpt.com/backend-api/codex/responses"
        .parse()
        .expect("a uri");
    let intercept = Matcher::builder()
        .https(url)
        .build()
        .intercept(&target)
        .expect("proxied");
    let auth = intercept
        .basic_auth()
        .map_or_default(|value| value.to_str().expect("ascii").to_string());
    tcr::request_key::ProxyCredential::from_connect_head(
        &format!("CONNECT chatgpt.com:443 HTTP/1.1\r\nProxy-Authorization: {auth}\r\n\r\n"),
        Some(KEY),
    )
}

/// Every variable the Codex shell policy sets, in order.
const SHELL_POLICY_VARS: [&str; 8] = [
    "HTTPS_PROXY",
    "https_proxy",
    "HTTP_PROXY",
    "http_proxy",
    "ALL_PROXY",
    "all_proxy",
    "NODE_EXTRA_CA_CERTS",
    "CODEX_CA_CERTIFICATE",
];

/// The `shell_environment_policy.set.*` arguments on `cmd`, as (name, value).
fn shell_policy(cmd: &std::process::Command) -> Vec<(String, String)> {
    cmd.get_args()
        .filter_map(|arg| {
            let arg = arg.to_string_lossy();
            let (name, value) = arg
                .strip_prefix("shell_environment_policy.set.")?
                .split_once('=')?;
            let value: String = serde_json::from_str(value).expect("a JSON string");
            Some((name.to_string(), value))
        })
        .collect()
}

/// Fail unless `cmd`'s shell policy sets exactly `want`, in [`SHELL_POLICY_VARS`]
/// order. The message names the variables that differ and never their values,
/// because a value may be a proxy URL that carries a request key.
fn assert_shell_policy(cmd: &std::process::Command, want: &[(&str, &str)], context: &str) {
    let got = shell_policy(cmd);
    let names: Vec<&str> = got.iter().map(|(name, _)| name.as_str()).collect();
    assert!(
        names == SHELL_POLICY_VARS,
        "{context}: shell policy names {names:?}"
    );
    let differ: Vec<&str> = got
        .iter()
        .filter(|(name, value)| {
            let want = want.iter().find(|(n, _)| n == name).map_or("", |(_, v)| *v);
            value != want
        })
        .map(|(name, _)| name.as_str())
        .collect();
    assert!(
        differ.is_empty(),
        "{context}: shell policy values differ for {differ:?}"
    );
}

/// Keyed model traffic retains the marker; shell defaults contain no key.
/// The caller's environment and Codex config are injected and empty, so the
/// result does not depend on the machine, or on a `tcr run` session around it.
#[test]
fn a_keyed_codex_launch_keeps_the_marker_and_restores_shell_settings() {
    let pooled = CodexLaunch::SeeThrough {
        ca: PathBuf::from("/tmp/ca.pem"),
        no_proxy: Vec::new(),
    };
    let caller = inherited(&[]);
    let host = test_host(&caller);

    let mut cmd = std::process::Command::new("true");
    let lines = apply_codex_launch(&mut cmd, 3999, &pooled, &sent(KEY), &host);
    let codex_url = format!("http://tcr-codex:{KEY}@127.0.0.1:3999");

    let env = env_changes(&cmd);
    for var in ["HTTPS_PROXY", "https_proxy"] {
        assert!(
            env.get(var) == Some(&Some(codex_url.clone())),
            "{var} is not the keyed codex URL"
        );
    }
    assert_shell_policy(&cmd, &[], "keyed");
    let codex = server_reads(&codex_url);
    assert!(codex.codex_marked && codex.key_ok, "{codex:?}");
    assert!(
        lines
            .iter()
            .any(|line| line.contains("caller's proxy and CA"))
    );

    for decision in unkeyed_decisions() {
        let mut cmd = std::process::Command::new("true");
        let lines = apply_codex_launch(&mut cmd, 3999, &pooled, &decision, &host);
        let env = env_changes(&cmd);
        assert!(
            env.get("HTTPS_PROXY") == Some(&Some("http://tcr-codex@127.0.0.1:3999".to_string())),
            "{decision:?}: HTTPS_PROXY is not the unkeyed codex URL"
        );
        assert_shell_policy(&cmd, &[], &format!("{decision:?}"));
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.contains("without the request key"))
                .count(),
            1,
            "{decision:?}: one notice says why: {lines:?}"
        );
    }

    let mut cmd = std::process::Command::new("true");
    let lines = apply_codex_launch(
        &mut cmd,
        3999,
        &CodexLaunch::Direct {
            reason: "no proxy answered on 127.0.0.1:3999".to_string(),
            unset: Vec::new(),
        },
        &sent(KEY),
        &host,
    );
    assert!(env_changes(&cmd).is_empty(), "a direct launch sets nothing");
    assert!(shell_policy(&cmd).is_empty());
    assert!(
        !lines.iter().any(|line| line.contains("request key")),
        "{lines:?}"
    );
}

/// The shell policy is built from the caller's environment and Codex config as
/// injected: a caller proxy aimed at this tcr (carrying its key) and tcr's own
/// CA are dropped, a caller's other proxy is kept, and a proxy the Codex config
/// sets wins.
#[test]
fn the_codex_shell_policy_reads_the_injected_environment_and_codex_config() {
    let home = tempfile::tempdir().expect("a temporary HOME");
    std::fs::create_dir(home.path().join(".codex")).expect("a .codex dir");
    std::fs::write(
        home.path().join(".codex/config.toml"),
        "[shell_environment_policy.set]\nALL_PROXY = \"http://config.example:8080\"\n",
    )
    .expect("a codex config");
    let home_text = home.path().to_str().expect("a UTF-8 path");
    let pooled = CodexLaunch::SeeThrough {
        ca: PathBuf::from("/tmp/ca.pem"),
        no_proxy: Vec::new(),
    };
    let caller = inherited(&[
        ("HOME", home_text),
        ("HTTPS_PROXY", "http://k:secret@127.0.0.1:3999"),
        ("http_proxy", "http://corp.example:3128"),
        ("NODE_EXTRA_CA_CERTS", "/tmp/ca.pem"),
    ]);
    let mut cmd = std::process::Command::new("true");
    let _lines = apply_codex_launch(&mut cmd, 3999, &pooled, &sent(KEY), &test_host(&caller));
    assert_shell_policy(
        &cmd,
        &[
            ("http_proxy", "http://corp.example:3128"),
            ("ALL_PROXY", "http://config.example:8080"),
        ],
        "HOME's codex config",
    );

    // CODEX_HOME names the config directory and wins over HOME.
    let codex_home = tempfile::tempdir().expect("a CODEX_HOME");
    std::fs::write(
        codex_home.path().join("config.toml"),
        "[shell_environment_policy.set]\nALL_PROXY = \"http://codex-home.example:8080\"\n",
    )
    .expect("a codex config");
    let caller = inherited(&[
        ("HOME", home_text),
        (
            "CODEX_HOME",
            codex_home.path().to_str().expect("a UTF-8 path"),
        ),
    ]);
    let mut cmd = std::process::Command::new("true");
    let _lines = apply_codex_launch(&mut cmd, 3999, &pooled, &sent(KEY), &test_host(&caller));
    assert_shell_policy(
        &cmd,
        &[("ALL_PROXY", "http://codex-home.example:8080")],
        "CODEX_HOME's codex config",
    );
}

/// (d): keyed, `x-tcr-key` is appended after the caller's and `--group`'s
/// headers, replacing an outer `tcr run`'s key line; unkeyed, the caller's
/// value is untouched.
#[test]
fn a_keyed_base_url_launch_appends_the_key_after_the_callers_headers() {
    let headers = |inherited: Option<&str>, key: &LaunchKey| {
        let mut cmd = std::process::Command::new("true");
        match inherited {
            Some(value) => cmd.env("ANTHROPIC_CUSTOM_HEADERS", value),
            None => cmd.env_remove("ANTHROPIC_CUSTOM_HEADERS"),
        };
        let _lines = apply_base_url_env(&mut cmd, 3999, key);
        env_changes(&cmd)
            .get("ANTHROPIC_CUSTOM_HEADERS")
            .cloned()
            .flatten()
    };
    let line = format!("x-tcr-key: {KEY}");
    assert_eq!(
        headers(Some("x-user: 1\nx-tcr-group: g"), &sent(KEY)),
        Some(format!("x-user: 1\nx-tcr-group: g\n{line}"))
    );
    assert_eq!(
        headers(Some("X-TCR-KEY: outer\nx-user: 1"), &sent(KEY)),
        Some(format!("x-user: 1\n{line}")),
        "a nested launch replaces the outer key line"
    );
    assert_eq!(headers(None, &sent(KEY)), Some(line));
    for decision in unkeyed_decisions() {
        assert_eq!(
            headers(Some("x-user: 1"), &decision),
            Some("x-user: 1".to_string()),
            "{decision:?}"
        );
        assert_eq!(headers(None, &decision), None, "{decision:?}");
    }
}

/// No stderr line a launch prints carries the key: the banners show the URL
/// with the key as `***`, and the notices name `tcr request-key` instead.
#[test]
fn no_launch_line_carries_the_request_key() {
    let key = sent(KEY);
    let ca = Path::new("/tmp/ca.pem");
    let mut lines = Vec::new();
    let mut cmd = std::process::Command::new("true");
    lines.extend(apply_see_through_env(&mut cmd, 3999, ca, &key));
    lines.extend(apply_base_url_env(&mut cmd, 3999, &key));
    for caller_ca in [false, true] {
        lines.extend(apply_codex_see_through_env(
            &mut cmd, 3999, ca, caller_ca, &key,
        ));
    }
    lines.extend(apply_codex_launch(
        &mut cmd,
        3999,
        &CodexLaunch::SeeThrough {
            ca: ca.to_path_buf(),
            no_proxy: Vec::new(),
        },
        &key,
        &test_host(&inherited(&[])),
    ));
    lines.extend(unkeyed_decisions().iter().filter_map(|d| d.notice(3999)));
    assert!(
        lines
            .iter()
            .any(|line| line.contains("via http://tcr:***@127.0.0.1:3999")),
        "control: the see-through banner names the redacted URL: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("via http://tcr-codex:***@127.0.0.1:3999")),
        "control: the codex banner names the redacted URL: {lines:?}"
    );
    for line in &lines {
        assert!(!line.contains(KEY), "a stderr line carries the key: {line}");
    }
}

/// The re-entry marker must reach the child, and must not be named such that
/// the wrapper it informs deletes it on the way.
///
/// cmux's claude wrapper runs `for k in ${!CMUX_@}; do unset "$k"; done` before
/// exec'ing the real binary. A marker under that prefix would be swept exactly
/// where it is needed, and the double-wrap would come back looking like a bug
/// in the launcher rather than in the name.
#[test]
fn marker_survives_the_cmux_prefix_sweep() {
    assert!(
        !RUN_ACTIVE_ENV.starts_with("CMUX_"),
        "{RUN_ACTIVE_ENV} would be erased by cmux's own CMUX_* sweep before the \
         launcher that reads it ever runs"
    );

    let mut cmd = claude_command();
    mark_run_active(&mut cmd);
    assert!(
        cmd.get_envs()
            .any(|(k, v)| k == RUN_ACTIVE_ENV && v == Some("1".as_ref())),
        "every `tcr run` child must carry {RUN_ACTIVE_ENV}"
    );
}

// ---- `--group` header composition (`compose_group_header`) ----------------

#[test]
fn group_header_with_no_inherited_value_is_just_ours() {
    assert_eq!(
        compose_group_header(None, "codereview"),
        "x-tcr-group: codereview"
    );
    // Empty is treated the same as absent.
    assert_eq!(
        compose_group_header(Some(""), "codereview"),
        "x-tcr-group: codereview"
    );
}

#[test]
fn group_header_appends_to_unrelated_inherited_headers() {
    let composed = compose_group_header(Some("X-Custom: yes"), "codereview");
    assert_eq!(composed, "X-Custom: yes\nx-tcr-group: codereview");
}

#[test]
fn group_header_replaces_rather_than_duplicates_an_existing_group_line() {
    let composed = compose_group_header(
        Some("X-Custom: yes\nx-tcr-group: stale\nX-Other: also-kept"),
        "codereview",
    );
    assert_eq!(
        composed, "X-Custom: yes\nX-Other: also-kept\nx-tcr-group: codereview",
        "the stale line is replaced in place at the END, never duplicated, \
         and every unrelated line's text and order survive"
    );
    assert_eq!(
        composed.matches("x-tcr-group").count(),
        1,
        "must never carry two group headers"
    );
}

#[test]
fn group_header_replacement_matches_the_name_case_insensitively() {
    let composed = compose_group_header(Some("X-TCR-Group: stale"), "codereview");
    assert_eq!(composed, "x-tcr-group: codereview");
}
