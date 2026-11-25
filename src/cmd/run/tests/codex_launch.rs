//! Tests of `tcr run --codex`: the pool preflight, the launch decision against what the running
//! proxy reports, and the proxy and `NO_PROXY` variables a launch keeps or drops.

use std::path::{Path, PathBuf};

use clap::Parser as _;
use tcr::doctor::{no_proxy_entry_exempts, points_at_tcr};
use tcr::{live_client, status};

use crate::cmd::run::launch_env::LaunchKey;
use crate::cmd::run::{
    CodexLaunch, NoProxyRewrite, RunArgs, apply_codex_launch, codex_launch, codex_pool_preflight,
    no_claude_account_warning, refuse_group_with_codex,
};
use crate::cmd::test_support::{env_changes, inherited, test_host};
use crate::{Cli, Command};

/// The [`RunArgs`] of a parsed `tcr run …`. `let … else` rather than a match arm that
/// formats the other variant: neither `Cli` nor `Command` implements Debug.
fn run_args_of(cli: Cli) -> RunArgs {
    let Some(Command::Run(args)) = cli.command else {
        panic!("expected the parsed command to be `tcr run`");
    };
    args
}

/// `--group` works by setting `ANTHROPIC_CUSTOM_HEADERS`, which Claude Code turns into a
/// request header and `codex` ignores, so with `--codex` the session would quietly route to
/// the whole Codex pool. clap accepts the pair; the launcher has to refuse it before
/// anything is spawned.
#[test]
fn run_codex_with_group_is_a_usage_error() {
    let err = Cli::try_parse_from(["tcr", "run", "--codex", "--group", "g", "--", "exec", "hi"]);
    // Parsing succeeds (clap cannot know), but run_harness must refuse before launching:
    let args = match err {
        Ok(cli) => cli,
        Err(e) => panic!("{e}"),
    };
    assert!(refuse_group_with_codex(&run_args_of(args)).is_err());
}

/// `tcr run --codex -- exec hi`, as the launch-decision tests run it.
const CODEX_RUN: &[&str] = &["tcr", "run", "--codex", "--", "exec", "hi"];

/// A parsed `tcr run …`'s [`RunArgs`].
fn run_args(argv: &[&str]) -> RunArgs {
    run_args_of(Cli::try_parse_from(argv).unwrap_or_else(|e| panic!("{argv:?} must parse: {e}")))
}

/// The running proxy's status as `live_client::fetch_live_status` hands it over:
/// parsed from the wire, so the tri-state `mitm` key is read the way a real
/// answer is. `None` leaves the key out, as a server older than it does.
/// Its pool holds one Codex account, without which the proxy intercepts no
/// `chatgpt.com` tunnel at all.
fn live_status(
    mitm: Option<serde_json::Value>,
) -> Result<status::StatusPayload, live_client::LiveStatusError> {
    let mut wire = serde_json::json!({
        "kind": status::STATUS_KIND,
        "accounts": [{
            "name": "bob@example.com/codex", "provider": "codex", "priority": 0,
            "status": "active", "disabled": false, "requests": 0, "inputTokens": 0,
            "outputTokens": 0, "cacheReadTokens": 0, "cacheCreationTokens": 0,
            "probeStatus": "ok", "quotaState": "normal", "gate": "ok", "threshold": 0.9,
        }],
        "build": {"sha": "abc1234"},
    });
    if let Some(mitm) = mitm {
        wire["mitm"] = mitm;
    }
    Ok(serde_json::from_value(wire).expect("a status payload"))
}

/// One row of [`status_with_rows`]: a provider, a name, a gate, and optionally a `freeAtMs`
/// and a `limitReachedType`.
type WireRow<'a> = (&'a str, &'a str, &'a str, Option<i64>, Option<&'a str>);

/// A running proxy's status, parsed from the wire, holding `rows`.
fn status_with_rows(rows: &[WireRow]) -> status::StatusPayload {
    let accounts: Vec<serde_json::Value> = rows
        .iter()
        .enumerate()
        .map(|(i, (provider, name, gate, free_at_ms, limit_reached))| {
            let mut row = serde_json::json!({
                "name": name, "priority": i, "status": "active",
                "disabled": *gate == "disabled", "requests": 0, "inputTokens": 0,
                "outputTokens": 0, "cacheReadTokens": 0, "cacheCreationTokens": 0,
                "probeStatus": "ok", "quotaState": "normal", "gate": gate, "threshold": 0.9,
            });
            if *provider == "codex" {
                row["provider"] = serde_json::json!("codex");
            }
            if let Some(at) = free_at_ms {
                row["freeAtMs"] = serde_json::json!(at);
            }
            if let Some(kind) = limit_reached {
                row["limitReachedType"] = serde_json::json!(kind);
            }
            row
        })
        .collect();
    serde_json::from_value(serde_json::json!({
        "kind": status::STATUS_KIND,
        "accounts": accounts,
        "build": {"sha": "abc1234"},
    }))
    .expect("a status payload")
}

/// 2026-10-14T00:00:00Z.
fn preflight_now() -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp(1_791_936_000).expect("a valid instant")
}

/// **`tcr run --codex` says, before its banner, when no Codex account can serve**: why each
/// is out, as tcr's own 429 lists its gates but in Codex words, and when the first frees.
/// One Codex account in rotation, or none in the pool at all, says nothing.
#[test]
fn the_run_codex_preflight_names_the_gates_and_the_soonest_return() {
    let payload = status_with_rows(&[
        ("anthropic", "alice@example.com", "ok", None, None),
        (
            "codex",
            "carol@example.com/codex",
            "seven-day",
            Some(1_792_072_800_000),
            None,
        ),
        ("codex", "erin@example.com/codex", "login", None, None),
    ]);
    assert_eq!(
        codex_pool_preflight(&payload, preflight_now()).as_deref(),
        Some(
            "[tcr] no Codex account can serve right now: 1 in the weekly window, 1 needing \
             re-login — the soonest frees at 2026-10-15 14:00:00 UTC"
        )
    );

    // An account held by an upstream 403 is out for a short hold, which is then the
    // soonest return, under a day away, so the clock alone.
    let payload = status_with_rows(&[
        (
            "codex",
            "carol@example.com/codex",
            "seven-day",
            Some(1_792_072_800_000),
            None,
        ),
        ("codex", "erin@example.com/codex", "login", None, None),
        (
            "codex",
            "frank@example.com/codex",
            "refused",
            Some(1_791_936_090_000),
            None,
        ),
    ]);
    assert_eq!(
        codex_pool_preflight(&payload, preflight_now()).as_deref(),
        Some(
            "[tcr] no Codex account can serve right now: 1 refused upstream (403), 1 in the \
             weekly window, 1 needing re-login — the soonest frees at 00:01:30 UTC"
        )
    );

    let serving = status_with_rows(&[
        (
            "codex",
            "carol@example.com/codex",
            "seven-day",
            Some(1_792_072_800_000),
            None,
        ),
        ("codex", "dana@example.com/codex", "ok", None, None),
    ]);
    assert_eq!(codex_pool_preflight(&serving, preflight_now()), None);
    let claude_only = status_with_rows(&[("anthropic", "alice@example.com", "login", None, None)]);
    assert_eq!(codex_pool_preflight(&claude_only, preflight_now()), None);
}

/// When no Codex account reports when it frees, the line says so rather than naming a time.
/// An account whose hold sits over ChatGPT's own refusal is counted as refused by ChatGPT:
/// the refusal outlasts the hold, and is why it stays out.
///
/// Watched red: with a disabled row counted as able to serve, this pool says nothing.
#[test]
fn the_run_codex_preflight_says_none_reports_when_it_frees() {
    let mut rows = vec![
        ("codex", "erin@example.com/codex", "login", None, None),
        ("codex", "gil@example.com/codex", "disabled", None, None),
    ];
    assert_eq!(
        codex_pool_preflight(&status_with_rows(&rows), preflight_now()).as_deref(),
        Some(
            "[tcr] no Codex account can serve right now: 1 needing re-login, 1 disabled — \
             none reports when it frees"
        )
    );
    rows.push((
        "codex",
        "hal@example.com/codex",
        "hold",
        None,
        Some("rate_limit_reached"),
    ));
    assert_eq!(
        codex_pool_preflight(&status_with_rows(&rows), preflight_now()).as_deref(),
        Some(
            "[tcr] no Codex account can serve right now: 1 needing re-login, 1 refused by \
             ChatGPT, 1 disabled — none reports when it frees"
        )
    );
}

/// **`tcr run` warns a Claude session that the running proxy holds no Claude account**, and
/// so will refuse every request it sends. Any Claude account, or an empty pool, says
/// nothing.
#[test]
fn run_warns_when_the_proxy_holds_no_claude_account() {
    let codex_only = status_with_rows(&[
        ("codex", "bob@example.com/codex", "ok", None, None),
        ("codex", "dana@example.com/codex", "disabled", None, None),
    ]);
    assert_eq!(
        no_claude_account_warning(3999, &codex_only).as_deref(),
        Some(
            "[tcr] warning: the proxy on :3999 holds no Claude account, so it will refuse \
             every request from this session — add one with `tcr login`"
        )
    );
    let mixed = status_with_rows(&[
        ("anthropic", "alice@example.com", "disabled", None, None),
        ("codex", "bob@example.com/codex", "ok", None, None),
    ]);
    assert_eq!(no_claude_account_warning(3999, &mixed), None);
    assert_eq!(
        no_claude_account_warning(3999, &status_with_rows(&[])),
        None
    );
}

/// Every row of `tcr run --codex`'s launch decision ([`codex_launch`]): the
/// launch it picks, and the exact notice it prints. Each row inherits an
/// `HTTPS_PROXY` aimed at this tcr, so every direct row must drop it, the
/// "not listening" one included: a dead tcr behind that variable refuses the
/// connection rather than launching anything.
///
/// Only the "key absent" row may tell the user to restart. MITM off survives
/// any restart, and saying otherwise sends the user to pay a
/// cold prompt cache for nothing.
///
/// Mismatches are collected rather than asserted one at a time, so a
/// regression names every row it breaks.
#[test]
fn codex_launch_follows_what_the_running_proxy_reports() {
    const PORT: u16 = 3999;
    const AT_TCR: &[(&str, &str)] = &[("HTTPS_PROXY", "http://127.0.0.1:3999")];
    let ca = Path::new("/tmp/tcr-run-test/tcr-ca.pem");
    let pooling = || {
        live_status(Some(serde_json::json!({
            "interceptedHosts": ["api.anthropic.com", "platform.claude.com", "chatgpt.com"],
            "leaf": "minted",
        })))
    };
    let direct = |reason: &str| {
        Ok((
            CodexLaunch::Direct {
                reason: reason.to_string(),
                unset: vec!["HTTPS_PROXY"],
            },
            vec![
                format!("[tcr] launching codex directly (not pooled): {reason}"),
                "[tcr] removed HTTPS_PROXY from codex's environment: it points at tcr on \
                 :3999, which this launch bypasses"
                    .to_string(),
            ],
        ))
    };
    // (label, argv, the status read, whether the CA is on disk, the launch and
    // notices wanted — or the refusal's text).
    type Live = Result<status::StatusPayload, live_client::LiveStatusError>;
    type Want = Result<(CodexLaunch, Vec<String>), &'static str>;
    let rows: Vec<(&str, &[&str], Live, bool, Want)> = vec![
        (
            "--group is refused before anything launches",
            &["tcr", "run", "--codex", "--group", "g", "--", "exec", "hi"],
            pooling(),
            true,
            Err("--group is not supported with --codex"),
        ),
        (
            "not listening",
            CODEX_RUN,
            Err(live_client::LiveStatusError::NoServer),
            true,
            direct("no proxy answered on 127.0.0.1:3999"),
        ),
        (
            "no answer",
            CODEX_RUN,
            Err(live_client::LiveStatusError::NoAnswer(
                "the server did not answer within 5s".to_string(),
            )),
            true,
            direct(
                "could not read the running proxy's status (the server did not answer \
                 within 5s)",
            ),
        ),
        (
            "an answer that is not a usable status",
            CODEX_RUN,
            Err(live_client::LiveStatusError::Unusable(
                "HTTP 401 Unauthorized — the proxy api-key in the config was rejected".to_string(),
            )),
            true,
            direct(
                "could not read the running proxy's status (HTTP 401 Unauthorized — the \
                 proxy api-key in the config was rejected)",
            ),
        ),
        (
            "key absent: a proxy older than the key",
            CODEX_RUN,
            live_status(None),
            true,
            direct(
                "the proxy on :3999 (build abc1234) predates Codex interception — restart \
                 it on this build at a quiet moment",
            ),
        ),
        (
            "MITM off",
            CODEX_RUN,
            live_status(Some(
                serde_json::json!({"interceptedHosts": [], "leaf": "none"}),
            )),
            true,
            direct(
                "the proxy on :3999 is running without MITM: its TLS material failed to \
                 load at boot — see its log",
            ),
        ),
        (
            "pooled",
            CODEX_RUN,
            pooling(),
            true,
            Ok((
                CodexLaunch::SeeThrough {
                    ca: ca.to_path_buf(),
                    no_proxy: Vec::new(),
                },
                Vec::new(),
            )),
        ),
        (
            "chatgpt.com intercepted, but no CA on disk",
            CODEX_RUN,
            pooling(),
            false,
            direct("no readable CA at /tmp/tcr-run-test/tcr-ca.pem"),
        ),
        (
            "a leaf that covers chatgpt.com, but no Codex account in the pool",
            CODEX_RUN,
            pooling().map(|mut payload| {
                payload
                    .accounts
                    .retain(|row| row.provider != tcr::provider::Provider::Codex);
                payload
            }),
            true,
            direct(
                "the proxy on :3999 holds no Codex account, so it blind-tunnels chatgpt.com \
                 and codex keeps its own login — add one with `tcr login --codex`",
            ),
        ),
        (
            "a minted leaf that does not cover chatgpt.com",
            CODEX_RUN,
            live_status(Some(serde_json::json!({
                "interceptedHosts": ["api.anthropic.com", "platform.claude.com"],
                "leaf": "minted",
            }))),
            true,
            direct(
                "the proxy on :3999 serves a leaf it minted but does not intercept \
                 chatgpt.com (it intercepts [api.anthropic.com, platform.claude.com])",
            ),
        ),
        (
            "a leaf this build does not know, whose CA it cannot name",
            CODEX_RUN,
            live_status(Some(serde_json::json!({
                "interceptedHosts": ["chatgpt.com"],
                "leaf": "future",
            }))),
            true,
            direct(
                "the proxy on :3999 (build abc1234) reports a leaf this build does not know \
                 (\"future\") — run tcr run --codex from the same build as the proxy",
            ),
        ),
    ];

    let mut wrong = Vec::new();
    for (label, argv, live, ca_is_file, want) in rows {
        let got = codex_launch(
            &run_args(argv),
            PORT,
            &live,
            ca,
            ca_is_file,
            &inherited(AT_TCR),
        )
        .map(|launch| {
            let notices = launch.notices(PORT);
            (launch, notices)
        })
        .map_err(|err| err.to_string());
        let right = match (&got, &want) {
            (Ok(got), Ok(want)) => got == want,
            (Err(got), Err(want)) => got.contains(want),
            _ => false,
        };
        if !right {
            wrong.push(format!("{label}:\n  got  {got:?}\n  want {want:?}"));
        }
    }
    assert!(
        wrong.is_empty(),
        "{} row(s) wrong:\n{}",
        wrong.len(),
        wrong.join("\n")
    );
}

#[test]
fn a_codex_launch_keeps_a_corporate_ca_and_runs_direct_when_tcr_is_absent() {
    let scratch = tempfile::tempdir().unwrap();
    let ca = scratch.path().join("tcr.pem");
    let corporate = scratch.path().join("corporate.pem");
    std::fs::write(&ca, "tcr certificate fixture").unwrap();
    std::fs::write(&corporate, "different corporate certificate fixture").unwrap();
    let live = live_status(Some(
        serde_json::json!({"interceptedHosts": ["chatgpt.com"], "leaf": "minted"}),
    ));
    for variable in ["CODEX_CA_CERTIFICATE", "SSL_CERT_FILE"] {
        let corporate = corporate.to_str().unwrap();
        let launch = codex_launch(
            &run_args(CODEX_RUN),
            3999,
            &live,
            &ca,
            true,
            &inherited(&[
                ("HTTPS_PROXY", "http://127.0.0.1:3999"),
                (variable, corporate),
            ]),
        )
        .unwrap();
        let CodexLaunch::Direct { reason, unset } = launch else {
            panic!("pooling without trust in both CAs must not break a working corporate setup");
        };
        assert!(reason.contains(variable));
        assert_eq!(unset, ["HTTPS_PROXY"]);
    }
}

#[test]
fn a_pooled_codex_launch_uses_the_callers_complete_ssl_bundle() {
    let scratch = tempfile::tempdir().unwrap();
    let ca = scratch.path().join("tcr.pem");
    let bundle = scratch.path().join("bundle.pem");
    let generated = rcgen::generate_simple_self_signed(vec!["example.test".into()]).unwrap();
    let pem = generated.cert.pem();
    std::fs::write(&ca, &pem).unwrap();
    std::fs::write(&bundle, format!("# caller bundle\n{pem}")).unwrap();
    let live = live_status(Some(
        serde_json::json!({"interceptedHosts": ["chatgpt.com"], "leaf": "minted"}),
    ));
    let launch = codex_launch(
        &run_args(CODEX_RUN),
        3999,
        &live,
        &ca,
        true,
        &inherited(&[("SSL_CERT_FILE", bundle.to_str().unwrap())]),
    )
    .unwrap();
    assert!(
        matches!(launch, CodexLaunch::SeeThrough {ca, ..} if ca == bundle),
        "setting CODEX_CA_CERTIFICATE must retain the caller's complete bundle"
    );
}

/// A direct launch drops only the proxy variables that point at THIS tcr
/// (loopback host, the configured port, scheme optional) and keeps every
/// other proxy: one on another port (a local px or cntlm) or on another host
/// (a corporate egress) may be how this machine reaches the internet at all,
/// and removing it would break a launch that only had to go un-pooled.
#[test]
fn a_direct_codex_launch_drops_only_the_proxy_variables_aimed_at_this_tcr() {
    let env = inherited(&[
        ("HTTPS_PROXY", "http://127.0.0.1:3999"),
        ("https_proxy", "localhost:3999"),
        ("HTTP_PROXY", "http://[::1]:3999/"),
        ("http_proxy", "http://127.0.0.1:8080"),
        ("ALL_PROXY", "http://proxy.corp.example:3999"),
        ("all_proxy", "socks5://127.0.0.1:1080"),
    ]);
    let launch = codex_launch(
        &run_args(CODEX_RUN),
        3999,
        &Err(live_client::LiveStatusError::NoServer),
        Path::new("/tmp/ca.pem"),
        false,
        &env,
    )
    .expect("a direct launch");
    let mut cmd = std::process::Command::new("true");
    let _lines = apply_codex_launch(
        &mut cmd,
        3999,
        &launch,
        &LaunchKey::NoKeyInConfig,
        &test_host(&env),
    );
    assert_eq!(
        env_changes(&cmd),
        [
            ("HTTPS_PROXY".to_string(), None),
            ("HTTP_PROXY".to_string(), None),
            ("https_proxy".to_string(), None),
        ]
        .into_iter()
        .collect(),
        "exactly the three aimed at tcr are removed, and nothing is set"
    );
    assert_eq!(
        launch.notices(3999).get(1).map(String::as_str),
        Some(
            "[tcr] removed HTTPS_PROXY, https_proxy, HTTP_PROXY from codex's environment: \
             they point at tcr on :3999, which this launch bypasses"
        )
    );

    for (value, aimed_at_tcr) in [
        ("http://127.0.0.1:3999", true),
        ("HTTP://LOCALHOST:3999/", true),
        ("https://[::1]:3999", true),
        ("127.0.0.1:3999", true),
        (" http://127.0.0.1:3999 ", true),
        ("socks5h://localhost:3999", true),
        ("http://127.0.0.1", false),
        ("http://127.0.0.1:39990", false),
        ("http://127.0.0.2:3999", false),
        ("http://proxy.corp.example:3999", false),
        ("", false),
        ("not a url", false),
    ] {
        assert_eq!(points_at_tcr(value, 3999), aimed_at_tcr, "{value:?}");
    }
}

/// A pooled launch drops only the `NO_PROXY` entries that would exempt chatgpt.com,
/// keeps the rest in order, keeps an emptied variable SET (codex reads the first
/// spelling that is set), and says what it dropped, so the banner it prints is true.
#[test]
fn a_pooled_codex_launch_drops_only_the_no_proxy_entries_that_exempt_chatgpt_com() {
    let env = inherited(&[
        ("NO_PROXY", "localhost,.chatgpt.com, 10.0.0.0/8"),
        ("no_proxy", "*"),
    ]);
    let live = live_status(Some(serde_json::json!({
        "interceptedHosts": ["chatgpt.com"],
        "leaf": "minted",
    })));
    let launch = codex_launch(
        &run_args(CODEX_RUN),
        3999,
        &live,
        Path::new("/tmp/ca.pem"),
        true,
        &env,
    )
    .expect("a pooled launch");
    assert_eq!(
        launch,
        CodexLaunch::SeeThrough {
            ca: PathBuf::from("/tmp/ca.pem"),
            no_proxy: vec![
                NoProxyRewrite {
                    var: "NO_PROXY",
                    kept: "localhost, 10.0.0.0/8".to_string(),
                    dropped: vec![".chatgpt.com".to_string()],
                },
                NoProxyRewrite {
                    var: "no_proxy",
                    kept: String::new(),
                    dropped: vec!["*".to_string()],
                },
            ],
        }
    );
    assert_eq!(
        launch.notices(3999),
        [
            "[tcr] dropped .chatgpt.com from NO_PROXY for codex: it exempts chatgpt.com from \
             the proxy, and the session would bypass tcr",
            "[tcr] dropped * from no_proxy for codex: it exempts chatgpt.com from the proxy, \
             and the session would bypass tcr",
        ]
    );

    let mut cmd = std::process::Command::new("true");
    let _lines = apply_codex_launch(
        &mut cmd,
        3999,
        &launch,
        &LaunchKey::NoKeyInConfig,
        &test_host(&env),
    );
    let env = env_changes(&cmd);
    assert_eq!(
        env.get("NO_PROXY"),
        Some(&Some("localhost, 10.0.0.0/8".to_string()))
    );
    assert_eq!(
        env.get("no_proxy"),
        Some(&Some(String::new())),
        "emptied, and still set"
    );
    assert_eq!(
        env.get("HTTPS_PROXY"),
        Some(&Some("http://tcr-codex@127.0.0.1:3999".to_string()))
    );
    assert_eq!(
        env.get("CODEX_CA_CERTIFICATE"),
        Some(&Some("/tmp/ca.pem".to_string()))
    );
}

/// [`no_proxy_entry_exempts`] against the matcher codex's HTTP stack runs:
/// hyper-util's, which reqwest builds its environment proxy from (reqwest
/// enables hyper-util's `client-proxy` feature for it; this tree and
/// `openai/codex` both lock hyper-util 0.1.20). An entry exempts chatgpt.com
/// exactly when that matcher sends `https://chatgpt.com/…` direct. The
/// expected column documents the rules; the matcher is the authority.
#[test]
fn no_proxy_matching_agrees_with_the_matcher_codex_uses() {
    use hyper_util::client::proxy::matcher::Matcher;
    let target: hyper::Uri = "https://chatgpt.com/backend-api/codex/responses"
        .parse()
        .expect("a uri");
    let proxied = |no_proxy: &str| {
        Matcher::builder()
            .https("http://127.0.0.1:3999")
            .no(no_proxy)
            .build()
            .intercept(&target)
            .is_some()
    };
    assert!(
        proxied(""),
        "control: with no NO_PROXY the request is proxied"
    );
    for (entry, exempts) in [
        ("chatgpt.com", true),
        ("CHATGPT.COM", true),
        (".chatgpt.com", true),
        (" chatgpt.com ", true),
        ("com", true),
        (".com", true),
        ("*", true),
        ("api.chatgpt.com", false),
        ("gpt.com", false),
        ("chatgpt.com.example", false),
        ("openai.com", false),
        ("*.chatgpt.com", false),
        (".", false),
        ("", false),
        ("10.0.0.0/8", false),
        ("127.0.0.1", false),
        ("::1", false),
    ] {
        assert_eq!(
            no_proxy_entry_exempts(entry, "chatgpt.com"),
            exempts,
            "{entry:?}"
        );
        assert_eq!(!proxied(entry), exempts, "the matcher on {entry:?}");
    }
}
