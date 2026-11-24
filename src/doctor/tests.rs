//! Tests for `tcr doctor`.

use super::*;

fn write(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create the settings directory");
    }
    std::fs::write(path, body).expect("write the settings file");
}

fn settings_with(url: &str) -> String {
    format!("{{\"env\": {{\"{BASE_URL_VAR}\": \"{url}\"}}}}")
}

/// The defect this verb exists for: the launcher exported the variable and
/// a settings file overrides it afterwards, so the FILE is the answer.
#[test]
fn a_settings_file_outranks_the_process_environment() {
    let home = tempfile::tempdir().expect("temp home");
    let cwd = tempfile::tempdir().expect("temp cwd");
    let settings = home.path().join(".claude").join("settings.json");
    write(&settings, &settings_with("https://gateway.example.com"));

    let resolved = resolve_base_url(Some("http://127.0.0.1:3456"), home.path(), cwd.path());

    assert_eq!(resolved.route.url, "https://gateway.example.com");
    assert_eq!(
        resolved.route.source,
        BaseUrlSource::UserSettings { path: settings }
    );
}

#[test]
fn the_project_settings_outrank_the_user_settings_and_local_outranks_both() {
    let home = tempfile::tempdir().expect("temp home");
    let cwd = tempfile::tempdir().expect("temp cwd");
    write(
        &home.path().join(".claude").join("settings.json"),
        &settings_with("https://user.example.com"),
    );
    write(
        &cwd.path().join(".claude").join("settings.json"),
        &settings_with("https://project.example.com"),
    );

    let project = resolve_base_url(None, home.path(), cwd.path());
    assert_eq!(project.route.url, "https://project.example.com");

    write(
        &cwd.path().join(".claude").join("settings.local.json"),
        &settings_with("https://local.example.com"),
    );
    let local = resolve_base_url(None, home.path(), cwd.path());
    assert_eq!(local.route.url, "https://local.example.com");
    assert_eq!(
        local.route.source,
        BaseUrlSource::ProjectLocalSettings {
            path: cwd.path().join(".claude").join("settings.local.json")
        }
    );
}

#[test]
fn the_environment_wins_when_no_settings_file_sets_one_and_the_default_is_last() {
    let home = tempfile::tempdir().expect("temp home");
    let cwd = tempfile::tempdir().expect("temp cwd");
    write(
        &home.path().join(".claude").join("settings.json"),
        "{\"model\": \"opus\"}",
    );

    let from_env = resolve_base_url(Some("http://127.0.0.1:9999"), home.path(), cwd.path());
    assert_eq!(from_env.route.source, BaseUrlSource::ProcessEnv);
    assert_eq!(from_env.route.url, "http://127.0.0.1:9999");

    let bare = resolve_base_url(None, home.path(), cwd.path());
    assert_eq!(bare.route.source, BaseUrlSource::Default);
    assert_eq!(bare.route.url, DEFAULT_BASE_URL);

    // An empty value is not a route. Claude Code ignores it, and reporting
    // it as the answer would name a source that decides nothing.
    let empty = resolve_base_url(Some("  "), home.path(), cwd.path());
    assert_eq!(empty.route.source, BaseUrlSource::Default);
}

/// A settings file that cannot be parsed is the likeliest hiding place for
/// the answer, so it is reported rather than read as "sets nothing".
#[test]
fn an_unreadable_settings_file_is_reported_and_resolution_continues() {
    let home = tempfile::tempdir().expect("temp home");
    let cwd = tempfile::tempdir().expect("temp cwd");
    write(
        &cwd.path().join(".claude").join("settings.json"),
        "{ not json",
    );
    write(
        &home.path().join(".claude").join("settings.json"),
        &settings_with("https://user.example.com"),
    );

    let resolved = resolve_base_url(None, home.path(), cwd.path());

    assert_eq!(resolved.route.url, "https://user.example.com");
    assert_eq!(resolved.problems.len(), 1, "{:?}", resolved.problems);
    assert!(
        resolved.problems[0].contains("settings.json"),
        "the problem must name the file: {}",
        resolved.problems[0]
    );
}

#[test]
fn a_non_string_base_url_is_a_problem_not_a_value() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("settings.json");
    write(&path, &format!("{{\"env\": {{\"{BASE_URL_VAR}\": 8080}}}}"));

    assert!(matches!(
        read_settings_base_url(&path),
        SettingsRead::Unreadable(_)
    ));
}

#[test]
fn authority_reads_host_and_port_including_ipv6_and_userinfo() {
    assert_eq!(
        authority("http://127.0.0.1:3456"),
        Some(("127.0.0.1".to_string(), 3456))
    );
    assert_eq!(
        authority("https://api.anthropic.com"),
        Some(("api.anthropic.com".to_string(), 443))
    );
    assert_eq!(
        authority("http://localhost/v1/messages?x=1"),
        Some(("localhost".to_string(), 80))
    );
    assert_eq!(
        authority("http://[::1]:3456/v1"),
        Some(("::1".to_string(), 3456))
    );
    assert_eq!(
        authority("http://user:pw@127.0.0.1:3456"),
        Some(("127.0.0.1".to_string(), 3456))
    );
    assert_eq!(authority("api.anthropic.com"), None);
    assert_eq!(authority("ftp://127.0.0.1:3456"), None);
}

#[test]
fn routing_is_loopback_plus_the_configured_port() {
    assert!(routes_to_port("http://127.0.0.1:3456", 3456));
    assert!(routes_to_port("http://localhost:3456", 3456));
    assert!(routes_to_port("http://[::1]:3456", 3456));
    assert!(!routes_to_port("http://127.0.0.1:3457", 3456));
    assert!(!routes_to_port("https://api.anthropic.com", 3456));
    // Another machine's tcr on the same port is still not this proxy.
    assert!(!routes_to_port("http://192.168.1.9:3456", 3456));
}

/// The enumeration half, against a listener this test process is holding:
/// the pid reported must be this process. A holder that reads as `none`
/// while something is demonstrably listening is the failure that makes the
/// whole `portHolder:` line worthless, and it cannot be caught by a test
/// that only feeds [`holder_from`] canned argv.
#[test]
fn port_holder_names_the_process_actually_listening() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a scratch port");
    let port = listener.local_addr().expect("local addr").port();

    let holder = port_holder(port).expect("a held port must have a holder");

    assert_eq!(holder.pid, std::process::id());
}

#[test]
fn the_holder_is_a_tcr_by_argv0_or_by_name() {
    let tcr = holder_from(11, "tcr", &["/usr/local/bin/tcr".into(), "server".into()]);
    assert!(tcr.is_tcr);
    let running_claude = holder_from(12, "tcr", &["/opt/tcr".into(), "run".into()]);
    assert!(running_claude.is_tcr, "`tcr run` hosts a proxy too");
    let stranger = holder_from(13, "node", &["/usr/bin/node".into(), "gateway.js".into()]);
    assert!(!stranger.is_tcr);
    assert_eq!(stranger.name, "node");
}

fn session(req_per_minute: Vec<u16>) -> tcr_status_wire::SessionRow {
    tcr_status_wire::SessionRow {
        session_id: "11111111-1111-1111-1111-111111111111".to_string(),
        account: None,
        model: None,
        first_seen_ms: 0,
        last_seen_ms: 0,
        requests: 0,
        input_tokens: 0,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
        tools: Default::default(),
        req_per_minute,
        cost_usd: 0.0,
        cache_ttl_secs: None,
        provider: None,
        unpriced_requests: 0,
        reasoning_tokens: 0,
    }
}

fn payload_with(sessions: Vec<tcr_status_wire::SessionRow>) -> StatusPayload {
    StatusPayload {
        kind: crate::status::STATUS_KIND.to_string(),
        accounts: Vec::new(),
        build: Default::default(),
        http1_only: false,
        control: None,
        group_colors: Default::default(),
        sessions,
        sessions_summary: Default::default(),
        peers: Vec::new(),
        peers_error: None,
        mitm: None,
        codex_traffic: None,
        codex_upstream: None,
        request_key_mode: None,
        unkeyed_connects: None,
        unkeyed_requests: None,
        heat: None,
        fan_control: None,
    }
}

/// The window is the TAIL of the sparkline (newest minutes), so a session
/// that was busy twenty minutes ago and idle since contributes nothing.
#[test]
fn the_window_counts_only_the_newest_ten_minutes() {
    let mut old_traffic = vec![0u16; 30];
    old_traffic[0] = 40;
    old_traffic[5] = 7;
    let mut recent = vec![0u16; 30];
    recent[29] = 3;
    recent[25] = 2;

    let payload = payload_with(vec![session(old_traffic), session(recent)]);

    assert_eq!(requests_in_window(&payload, REQUEST_WINDOW_MINUTES), 5);
}

#[test]
fn a_short_or_absent_sparkline_counts_what_it_has() {
    let payload = payload_with(vec![session(vec![1, 2]), session(Vec::new())]);
    assert_eq!(requests_in_window(&payload, REQUEST_WINDOW_MINUTES), 3);
}

/// `requests10m` answers "is Claude Code reaching this proxy", so it counts Claude sessions
/// only. A proxy busy with codex says nothing about Claude's route, and counting its
/// sessions would read a broken Claude route as healthy.
#[test]
fn requests_in_the_window_count_claude_sessions_only() {
    let mut claude = vec![0u16; 30];
    claude[29] = 5;
    let mut codex_traffic = vec![0u16; 30];
    codex_traffic[29] = 7;
    let mut codex = session(codex_traffic);
    codex.provider = Some("codex".to_string());
    let payload = payload_with(vec![session(claude), codex]);
    assert_eq!(requests_in_window(&payload, REQUEST_WINDOW_MINUTES), 5);
}

fn report_for(url: &str, source: BaseUrlSource, requests: Option<u64>) -> Report {
    Report {
        base_url: url.to_string(),
        base_url_source: source,
        proxy_port: 3456,
        routed_here: routes_to_port(url, 3456),
        port_holder: None,
        requests_in_window: requests,
        problems: Vec::new(),
        unknown_keys: Vec::new(),
        shell_route: None,
    }
}

/// `tcr setup claude-settings` writes a value doctor must call on-route.
#[test]
fn the_every_launch_value_is_on_route() {
    let url = crate::setup::proxy_base_url(3456);
    let report = report_for(
        &url,
        BaseUrlSource::UserSettings {
            path: PathBuf::from("/home/alice/.claude/settings.json"),
        },
        Some(2),
    );
    assert!(report.routed_here);
    assert_eq!(report.exit_code(), 0);
}

/// The shell route routes a typed `claude` when no settings file names a
/// base URL, and does not when one does.
#[test]
fn the_shell_route_is_on_route_unless_a_settings_file_overrides_it() {
    let rc = PathBuf::from("/home/alice/.zshrc");
    let mut report = report_for(DEFAULT_BASE_URL, BaseUrlSource::Default, Some(2));
    assert_eq!(report.exit_code(), 2, "no shell route: off route");
    report.shell_route = Some(rc.clone());
    assert_eq!(report.exit_code(), 0);
    assert!(
        report.verdict().contains("/home/alice/.zshrc"),
        "{}",
        report.verdict()
    );

    let mut silent = report_for(DEFAULT_BASE_URL, BaseUrlSource::Default, None);
    silent.shell_route = Some(rc.clone());
    assert_eq!(silent.exit_code(), 3);

    let mut overridden = report_for(
        "https://gateway.example.com",
        BaseUrlSource::UserSettings {
            path: PathBuf::from("/home/alice/.claude/settings.json"),
        },
        Some(2),
    );
    overridden.shell_route = Some(rc);
    assert_eq!(
        overridden.exit_code(),
        2,
        "a settings file outranks tcr run's export"
    );
}

#[test]
fn a_route_to_another_gateway_exits_two_and_names_the_file() {
    let home = tempfile::tempdir().expect("temp home");
    let settings = home.path().join(".claude").join("settings.json");
    let report = report_for(
        "https://gateway.example.com",
        BaseUrlSource::UserSettings {
            path: settings.clone(),
        },
        None,
    );

    assert_eq!(report.exit_code(), 2);
    assert_eq!(
        report.verdict(),
        format!(
            "verdict: this proxy is not on Claude's route: {} sets https://gateway.example.com",
            settings.display()
        )
    );
}

#[test]
fn a_route_here_with_a_serving_proxy_exits_zero() {
    let report = report_for("http://127.0.0.1:3456", BaseUrlSource::ProcessEnv, Some(4));

    assert_eq!(report.exit_code(), 0);
    assert_eq!(
        report.verdict(),
        "verdict: Claude is routed to http://127.0.0.1:3456 (ANTHROPIC_BASE_URL); this proxy has served 4 requests"
    );
}

/// Zero served is NOT the same fact as nothing answering, and the exit code
/// has to keep them apart: a proxy that is up and idle is healthy.
#[test]
fn nothing_answering_exits_three_and_zero_served_does_not() {
    let silent = report_for("http://127.0.0.1:3456", BaseUrlSource::ProcessEnv, None);
    assert_eq!(silent.exit_code(), 3);
    assert!(silent.verdict().contains("no proxy answered"));

    let idle = report_for("http://127.0.0.1:3456", BaseUrlSource::ProcessEnv, Some(0));
    assert_eq!(idle.exit_code(), 0);
}

#[test]
fn the_text_block_is_greppable_and_the_json_carries_the_derived_fields() {
    let mut report = report_for("http://127.0.0.1:3456", BaseUrlSource::ProcessEnv, Some(2));
    report.port_holder = Some(holder_from(77, "tcr", &["/usr/local/bin/tcr".into()]));
    report
        .problems
        .push("settings.json: not readable as JSON".to_string());

    let text = report.render_text();
    assert!(text.contains("baseUrl: http://127.0.0.1:3456"), "{text}");
    assert!(text.contains("baseUrlSource: ANTHROPIC_BASE_URL"), "{text}");
    assert!(text.contains("portHolder: pid 77 tcr"), "{text}");
    assert!(text.contains("portHolderIsTcr: yes"), "{text}");
    assert!(text.contains("requests10m: 2"), "{text}");
    assert!(
        text.contains("problem: settings.json: not readable as JSON"),
        "{text}"
    );
    assert!(
        text.lines()
            .last()
            .is_some_and(|last| last.starts_with("verdict: ")),
        "the verdict is the last line: {text}"
    );

    let json = report.to_json();
    assert_eq!(json["exitCode"], 0);
    assert_eq!(json["requestsInWindow"], 2);
    assert_eq!(json["baseUrl"], "http://127.0.0.1:3456");
    assert_eq!(json["portHolder"]["isTcr"], true);
    assert_eq!(json["requestWindowMinutes"], 10);
    assert!(
        json["verdict"]
            .as_str()
            .is_some_and(|v| v.starts_with("verdict: "))
    );
    assert_eq!(
        json["unknownKeys"],
        serde_json::json!([]),
        "an empty array, never absent or null, when every key is known"
    );
}

fn config_from(json: &str) -> crate::config::Config {
    serde_json::from_str(json).expect("a test config parses")
}

/// The lane's gate, in Rust: a misspelled `quotaProbeSeconds` is named in
/// the JSON and in the text, with the spelling it meant, and the exit code
/// stays the routing answer.
#[test]
fn a_misspelled_config_key_is_named_and_the_exit_code_is_unchanged() {
    let report = report_for("http://127.0.0.1:3456", BaseUrlSource::ProcessEnv, Some(0))
        .with_config_keys(&config_from(r#"{"accounts":[],"quotaProbeSecs":60}"#));

    assert_eq!(
        report.to_json()["unknownKeys"],
        serde_json::json!([{"key": "quotaProbeSecs", "nearest": "quotaProbeSeconds"}])
    );
    let text = report.render_text();
    assert!(
        text.contains("unknownKey: quotaProbeSecs (did you mean quotaProbeSeconds?)"),
        "{text}"
    );
    assert_eq!(
        report.exit_code(),
        0,
        "an unknown key is not a routing failure"
    );
}

#[test]
fn the_correct_spelling_reports_no_unknown_keys() {
    let report = report_for("http://127.0.0.1:3456", BaseUrlSource::ProcessEnv, Some(0))
        .with_config_keys(&config_from(r#"{"accounts":[],"quotaProbeSeconds":60}"#));

    assert_eq!(report.to_json()["unknownKeys"], serde_json::json!([]));
    assert!(!report.render_text().contains("unknownKey:"));
}

/// `tcr doctor` on a config holding a Codex account prints no `unknownKey:`
/// line and reports `unknownKeys: []`. Loaded the way `cli::doctor` loads it,
/// through `config::load`, so the Codex row's `credentialVersion` 2 is
/// accepted first, and with a `codexUpstream` the user set, so the key is on
/// disk. `codexUpstream` and each row's `provider` are modelled fields, so
/// serde takes them before the unmodelled map doctor reads is built, and
/// `codexUpstream` is in the registry besides.
///
/// Watched red: rename the `codexUpstream` field's serde key so the key lands
/// in that map AND drop its registry line, and it is named here. Either alone
/// stays green, the other one still covering it.
#[test]
fn a_codex_config_reports_no_unknown_key() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(crate::paths::CONFIG_FILE);
    std::fs::write(
        &path,
        r#"{"codexUpstream":"http://127.0.0.1:9100","accounts":[
                 {"name":"alice@example.com","accessToken":"at-a"},
                 {"name":"alice@example.com/codex","provider":"codex","type":"oauth",
                  "accessToken":"at-c","accountUuid":"11111111-1111-4111-8111-111111111111",
                  "credentialVersion":2}]}"#,
    )
    .expect("write the config");
    let config = crate::config::load(&path).expect("a Codex config loads");
    let report = report_for("http://127.0.0.1:3456", BaseUrlSource::ProcessEnv, Some(0))
        .with_config_keys(&config);

    assert_eq!(report.to_json()["unknownKeys"], serde_json::json!([]));
    let text = report.render_text();
    assert!(!text.contains("unknownKey:"), "{text}");
}

// --- the Codex half -------------------------------------------------------

use crate::stats::GateReason;
use tcr_status_wire::{CodexRoute, CodexTraffic};

fn pooled() -> CodexRoute {
    CodexRoute {
        pooled: true,
        code: None,
        reason: None,
        upstream: None,
    }
}

fn not_pooled(code: &str, reason: &str) -> CodexRoute {
    CodexRoute {
        pooled: false,
        code: Some(code.to_string()),
        reason: Some(reason.to_string()),
        upstream: None,
    }
}

/// A Codex report for a proxy on :3999 that answered: no Codex row yet, tcr's CA readable,
/// the default upstream, codex on `PATH`, nothing set in this shell, codex signed in, and
/// nothing served or refused.
fn codex_report(route: CodexRoute) -> CodexReport {
    CodexReport {
        port: 3999,
        rows: Vec::new(),
        route,
        ca: PathBuf::from("/Users/example/.config/tcr-ca.pem"),
        ca_readable: true,
        upstream: "https://chatgpt.com".to_string(),
        upstream_is_default: true,
        bin: Ok((PathBuf::from("/opt/homebrew/bin/codex"), "PATH".to_string())),
        env: CodexEnv::default(),
        login: PathBuf::from("/Users/example/.codex/auth.json"),
        login_exists: true,
        requests_10m: Some(0),
        traffic: Some(CodexTraffic::default()),
    }
}

fn codex_row(name: &str, gate: Option<GateReason>) -> CodexRow {
    CodexRow {
        name: name.to_string(),
        gate,
        reserved_groups: Vec::new(),
        limit_reached_type: None,
    }
}

/// The nine Codex rows of a mixed fleet, one per state, in a shuffled order: kim reserved
/// for research, hal held, gil refused and carol out for the week, the last three carrying
/// the limit ChatGPT named.
fn nine_rows() -> Vec<CodexRow> {
    let limited = |mut row: CodexRow| {
        row.limit_reached_type = Some("rate_limit_reached".to_string());
        row
    };
    vec![
        CodexRow {
            reserved_groups: vec!["research".to_string()],
            ..codex_row("kim@example.com", Some(GateReason::Reserved))
        },
        codex_row("bob@example.com/codex", Some(GateReason::Ok)),
        codex_row("erin@example.com/codex", Some(GateReason::Login)),
        limited(codex_row("hal@example.com", Some(GateReason::Hold))),
        codex_row("dana@example.com", Some(GateReason::Ok)),
        limited(codex_row("carol@example.com", Some(GateReason::SevenDay))),
        limited(codex_row("gil@example.com", Some(GateReason::Rejected))),
        codex_row("ivy@example.com", Some(GateReason::FiveHour)),
        codex_row("jo@example.com", Some(GateReason::Ok)),
    ]
}

/// `codexAccounts` counts the rows by gate in `GateReason` order and in the Codex words of
/// tcr's own 429. A row held by an upstream hold or 403 that carries ChatGPT's own refusal
/// counts as refused by ChatGPT, since that refusal is why it stays out; a window keeps its
/// row. `byGate` stays keyed on the wire's gate.
#[test]
fn codex_accounts_count_rows_by_gate_phrase() {
    use GateReason::{FiveHour, Hold, Login, Ok, Refused, Rejected, Reserved, SevenDay};
    let gated = |gates: &[GateReason]| -> Vec<CodexRow> {
        gates
            .iter()
            .enumerate()
            .map(|(i, gate)| codex_row(&format!("row{i}@example.com"), Some(*gate)))
            .collect()
    };
    let report = |rows: Vec<CodexRow>| CodexReport {
        rows,
        ..codex_report(pooled())
    };

    let shuffled = gated(&[
        Reserved, Ok, Login, Hold, Ok, SevenDay, Rejected, FiveHour, Ok,
    ]);
    assert_eq!(
        report(shuffled.clone()).accounts_line(),
        "9 (3 in rotation, 1 on an upstream hold, 1 in the 5-hour window, 1 in the weekly \
             window, 1 needing re-login, 1 refused by ChatGPT, 1 unreachable in a reserved group)"
    );
    assert_eq!(
        report(shuffled.clone()).to_json()["accounts"],
        serde_json::json!({"total": 9, "byGate": {
            "ok": 3, "hold": 1, "five-hour": 1, "seven-day": 1, "login": 1, "rejected": 1,
            "reserved": 1
        }})
    );

    let mut held_by_chatgpt = shuffled;
    held_by_chatgpt[3].limit_reached_type = Some("rate_limit_reached".to_string());
    assert_eq!(
        report(held_by_chatgpt).accounts_line(),
        "9 (3 in rotation, 1 in the 5-hour window, 1 in the weekly window, 1 needing \
             re-login, 2 refused by ChatGPT, 1 unreachable in a reserved group)"
    );

    assert_eq!(
        report(gated(&[Ok, Ok])).accounts_line(),
        "2 (2 in rotation)"
    );
    let offline = report(vec![
        codex_row("bob@example.com/codex", None),
        codex_row("dana@example.com", None),
    ]);
    assert_eq!(offline.accounts_line(), "2 (no proxy answered)");
    assert_eq!(
        offline.to_json()["accounts"],
        serde_json::json!({"total": 2, "byGate": {}})
    );
    assert_eq!(report(Vec::new()).accounts_line(), "0");

    // The 403 hold, in the words tcr's own 429 gives it, counted after the hold.
    let refused = report(gated(&[Ok, Refused]));
    assert_eq!(
        refused.accounts_line(),
        "2 (1 in rotation, 1 refused upstream (403))"
    );
    assert_eq!(
        refused.to_json()["accounts"]["byGate"],
        serde_json::json!({"ok": 1, "refused": 1})
    );
    let mut refused_by_chatgpt = gated(&[Ok, Refused]);
    refused_by_chatgpt[1].limit_reached_type = Some("rate_limit_reached".to_string());
    let refused_by_chatgpt = report(refused_by_chatgpt);
    assert_eq!(
        refused_by_chatgpt.accounts_line(),
        "2 (1 in rotation, 1 refused by ChatGPT)"
    );
    assert_eq!(
        refused_by_chatgpt.to_json()["accounts"]["byGate"],
        serde_json::json!({"ok": 1, "refused": 1})
    );
}

/// The Codex block's lines, keys in a fixed order and one problem per line, as a mixed
/// fleet's doctor prints them; then each line's other forms, one change at a time.
#[test]
fn the_codex_block_lines_are_in_order() {
    let base = CodexReport {
        rows: nine_rows(),
        requests_10m: Some(42),
        traffic: Some(CodexTraffic {
            refused_10m: 12,
            unpooled_10m: 3,
            ..CodexTraffic::default()
        }),
        ..codex_report(pooled())
    };
    let lines = base.block_lines();
    assert_eq!(
        lines,
        [
            "codexAccounts: 9 (3 in rotation, 1 in the 5-hour window, 1 in the weekly \
                 window, 1 needing re-login, 2 refused by ChatGPT, 1 unreachable in a reserved \
                 group)",
            "codexIntercept: yes",
            "codexCa: /Users/example/.config/tcr-ca.pem (readable)",
            "codexUpstream: https://chatgpt.com (default)",
            "codexBin: /opt/homebrew/bin/codex (PATH)",
            "codexEnv: HTTPS_PROXY unset · NO_PROXY unset · SSL_CERT_FILE unset",
            "codexLogin: /Users/example/.codex/auth.json exists (existence only; never read)",
            "codexRequests10m: 42",
            "codexRefused10m: 12",
            "codexUnpooled10m: 3",
            "codexProblem: kim@example.com is reserved for research, which no Codex request \
                 can ask for, so it serves nothing",
        ]
    );
    // The base block, with the line keyed `key` replaced by `line`.
    let one_change = |key: &str, line: &str| -> Vec<String> {
        lines
            .iter()
            .map(|old| {
                if old.starts_with(&format!("{key}: ")) {
                    line.to_string()
                } else {
                    old.clone()
                }
            })
            .collect()
    };

    let unreadable = CodexReport {
        ca_readable: false,
        ..base.clone()
    };
    assert_eq!(
        unreadable.block_lines(),
        one_change(
            "codexCa",
            "codexCa: /Users/example/.config/tcr-ca.pem (not readable)"
        )
    );

    let elsewhere = CodexReport {
        upstream: "http://127.0.0.1:9".to_string(),
        upstream_is_default: false,
        ..base.clone()
    };
    assert_eq!(
        elsewhere.block_lines(),
        one_change(
            "codexUpstream",
            "codexUpstream: http://127.0.0.1:9 (set by codexUpstream)"
        )
    );

    let searched = CodexReport {
        bin: Err(crate::codex_local::CodexSearch::NotFound {
            searched: vec![
                "TCR_CODEX_BIN".to_string(),
                "CODEX_BIN".to_string(),
                "PATH".to_string(),
            ],
        }),
        ..base.clone()
    };
    assert_eq!(
        searched.block_lines(),
        one_change(
            "codexBin",
            "codexBin: not found (searched TCR_CODEX_BIN, CODEX_BIN, PATH)"
        )
    );
    let bad_variable = CodexReport {
        bin: Err(crate::codex_local::CodexSearch::BadVariable {
            var: "TCR_CODEX_BIN",
            value: "/Users/example/plain".to_string(),
            bare: false,
        }),
        ..base.clone()
    };
    assert_eq!(
        bad_variable.block_lines(),
        one_change(
            "codexBin",
            "codexBin: not found (TCR_CODEX_BIN is /Users/example/plain, which is not an \
                 executable file)"
        )
    );
    let bare_name = CodexReport {
        bin: Err(crate::codex_local::CodexSearch::BadVariable {
            var: "CODEX_BIN",
            value: "codex-beta".to_string(),
            bare: true,
        }),
        ..base.clone()
    };
    assert_eq!(
        bare_name.block_lines(),
        one_change(
            "codexBin",
            "codexBin: not found (CODEX_BIN is codex-beta, and no executable codex-beta is \
                 on PATH)"
        )
    );

    let signed_out = CodexReport {
        login_exists: false,
        ..base.clone()
    };
    let mut expected = one_change(
        "codexLogin",
        "codexLogin: /Users/example/.codex/auth.json does not exist (existence only; never \
             read)",
    );
    expected.push(
            "codexProblem: auth.json is absent; a keyring-backed Codex login is not checked. Run codex \
             once to verify its own sign-in before using the pool"
                .to_string(),
        );
    assert_eq!(signed_out.block_lines(), expected);

    let unreported = CodexReport {
        traffic: None,
        ..base.clone()
    };
    let mut expected = one_change(
        "codexRefused10m",
        "codexRefused10m: n/a (the proxy does not report it)",
    );
    let unpooled = expected
        .iter()
        .position(|line| line.starts_with("codexUnpooled10m: "))
        .expect("an unpooled line");
    expected[unpooled] = "codexUnpooled10m: n/a (the proxy does not report it)".to_string();
    assert_eq!(unreported.block_lines(), expected);

    let offline = CodexReport {
        requests_10m: None,
        traffic: None,
        route: not_pooled("no-proxy", "no proxy answered on 127.0.0.1:3999"),
        ..base
    };
    let offline_lines = offline.block_lines();
    for line in [
        "codexIntercept: unknown",
        "codexRequests10m: none (no proxy answered)",
        "codexRefused10m: none (no proxy answered)",
        "codexUnpooled10m: none (no proxy answered)",
    ] {
        assert!(
            offline_lines.iter().any(|held| held == line),
            "{line:?} missing from {offline_lines:#?}"
        );
    }
    assert_eq!(
        offline_lines.last().map(String::as_str),
        Some("codexProblem: no proxy answered on 127.0.0.1:3999")
    );
}

/// A proxy variable in this shell that points at tcr sends a codex started here, outside
/// `tcr run --codex`, into tcr's interception without its CA. Only the variable codex itself
/// reads for an `https://` URL decides, and the problem is dropped when that codex goes
/// around tcr (`NO_PROXY`), trusts tcr's CA, or meets no interception at all.
#[test]
fn an_older_proxy_uses_configured_codex_rows_for_the_restart_diagnosis() {
    let payload: StatusPayload = serde_json::from_value(serde_json::json!({
        "kind": crate::status::STATUS_KIND, "accounts": [], "build": {"sha":"abc1234"}
    }))
    .unwrap();
    let config: crate::config::Config = serde_json::from_value(serde_json::json!({
            "accounts": [{"name":"alice@example.com/codex", "provider":"codex", "accessToken":"at-fake"}]
        })).unwrap();
    let rows = codex_rows(Ok(&payload), &config);
    assert_eq!(
        rows.len(),
        1,
        "an older status payload cannot report configured Codex rows"
    );
    assert_eq!(rows[0].name, "alice@example.com/codex");
    assert_eq!(
        rows[0].gate, None,
        "configuration is not a measured live gate"
    );
}

#[test]
fn codex_ca_trust_accepts_a_bundle_containing_the_tcr_certificate() {
    let scratch = tempfile::tempdir().unwrap();
    let ca = scratch.path().join("tcr.pem");
    let bundle = scratch.path().join("bundle.pem");
    let unrelated = scratch.path().join("other.pem");
    let generated = rcgen::generate_simple_self_signed(vec!["example.test".into()]).unwrap();
    let other = rcgen::generate_simple_self_signed(vec!["other.example.test".into()]).unwrap();
    std::fs::write(&ca, generated.cert.pem()).unwrap();
    std::fs::write(
        &bundle,
        format!("{}\n{}", other.cert.pem(), generated.cert.pem()),
    )
    .unwrap();
    std::fs::write(&unrelated, other.cert.pem()).unwrap();
    for explicit_codex in [false, true] {
        let mut env = CodexEnv::default();
        if explicit_codex {
            env.codex_ca_certificate = Some(bundle.display().to_string());
        } else {
            env.ssl_cert_file = Some(bundle.display().to_string());
        }
        assert!(
            env.trusts(&ca),
            "a combined bundle trusts the exact certificate"
        );
        env.codex_ca_certificate = Some(unrelated.display().to_string());
        assert!(
            !env.trusts(&ca),
            "CODEX_CA_CERTIFICATE takes precedence over SSL_CERT_FILE"
        );
    }
}

#[test]
fn codex_doctor_does_not_call_an_unchecked_keyring_login_missing() {
    let report = CodexReport {
        login_exists: false,
        ..codex_report(pooled())
    };
    let problems = report.problems().join(" ");
    assert!(
        problems.contains("keyring"),
        "the login may use Codex's keyring store: {problems}"
    );
    assert!(
        !problems.contains("codex login"),
        "do not revoke a possibly healthy keyring login: {problems}"
    );
}

#[test]
fn an_unmarked_codex_proxy_reports_how_to_join_the_pool() {
    let report = CodexReport {
        env: CodexEnv {
            https_proxy: Some(("HTTPS_PROXY".into(), "http://127.0.0.1:3999".into())),
            ..CodexEnv::default()
        },
        ..codex_report(pooled())
    };
    let problem = &report.problems()[0];
    assert!(problem.contains("unmarked"), "{problem}");
    assert!(problem.contains("tcr run --codex"), "{problem}");
    assert!(
        !problem.contains("tcr-codex@"),
        "do not mark the whole shell: {problem}"
    );
    assert!(!problem.contains("fails TLS"), "{problem}");
}

#[test]
fn a_codex_problem_names_https_proxy_pointing_at_tcr() {
    const AT_TCR: &str = "http://tcr-codex@127.0.0.1:3999";
    const CA: &str = "/Users/example/.config/tcr-ca.pem";
    const PROBLEM: &str = "HTTPS_PROXY in this shell points at tcr with the Codex marker \
                               but without its CA, so codex can fail TLS: use `tcr run --codex` \
                               with a CA bundle that includes tcr's CA; keep the marker scoped \
                               to that launch";
    let with_env = |route: CodexRoute, env: CodexEnv| CodexReport {
        env,
        ..codex_report(route)
    };
    let from_env = |vars: &[(&str, &str)]| {
        let vars: Vec<(String, String)> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        CodexEnv::from_env(&move |name| {
            vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
        })
    };
    let at_tcr = CodexEnv {
        https_proxy: Some(("HTTPS_PROXY".to_string(), AT_TCR.to_string())),
        ..CodexEnv::default()
    };
    let names_tcr = |report: &CodexReport| {
        report
            .problems()
            .iter()
            .any(|problem| problem.contains("in this shell points at tcr"))
    };
    let env_line = |report: &CodexReport| {
        report
            .block_lines()
            .into_iter()
            .find(|line| line.starts_with("codexEnv: "))
            .expect("an env line")
    };

    let report = with_env(pooled(), at_tcr.clone());
    assert_eq!(report.problems()[0], PROBLEM);
    assert_eq!(
        env_line(&report),
        "codexEnv: HTTPS_PROXY=http://…@127.0.0.1:3999 (points at tcr) · NO_PROXY unset · \
             SSL_CERT_FILE unset"
    );

    let exempt = with_env(
        pooled(),
        CodexEnv {
            no_proxy: Some(("NO_PROXY".to_string(), "chatgpt.com".to_string())),
            ..at_tcr.clone()
        },
    );
    assert!(!names_tcr(&exempt), "{:?}", exempt.problems());
    assert_eq!(
        env_line(&exempt),
        "codexEnv: HTTPS_PROXY=http://…@127.0.0.1:3999 (points at tcr) · \
             NO_PROXY=chatgpt.com (exempts chatgpt.com) · SSL_CERT_FILE unset"
    );
    for (label, env) in [
        (
            "CODEX_CA_CERTIFICATE names tcr's CA",
            CodexEnv {
                codex_ca_certificate: Some(CA.to_string()),
                ..at_tcr.clone()
            },
        ),
        (
            "SSL_CERT_FILE names tcr's CA and nothing overrides it",
            CodexEnv {
                ssl_cert_file: Some(CA.to_string()),
                ..at_tcr.clone()
            },
        ),
        (
            "the proxy is another one",
            CodexEnv {
                https_proxy: Some((
                    "HTTPS_PROXY".to_string(),
                    "http://127.0.0.1:8080".to_string(),
                )),
                ..CodexEnv::default()
            },
        ),
    ] {
        assert!(
            !names_tcr(&with_env(pooled(), env)),
            "no problem when {label}"
        );
    }
    assert!(
        !names_tcr(&with_env(
            not_pooled(
                "mitm-off",
                "the proxy on :3999 is running without MITM: its TLS material failed to \
                     load at boot — see its log"
            ),
            at_tcr.clone()
        )),
        "a proxy that intercepts nothing of codex's cannot break its TLS"
    );

    // codex reads SSL_CERT_FILE only while CODEX_CA_CERTIFICATE is unset.
    let overridden = with_env(
        pooled(),
        CodexEnv {
            ssl_cert_file: Some(CA.to_string()),
            codex_ca_certificate: Some("/etc/other.pem".to_string()),
            ..at_tcr.clone()
        },
    );
    assert_eq!(overridden.problems()[0], PROBLEM);

    // Only the first of codex's four spellings that is set decides.
    let all_proxy = with_env(pooled(), from_env(&[("ALL_PROXY", AT_TCR)]));
    assert!(
        all_proxy.problems()[0].starts_with("ALL_PROXY in this shell points at tcr"),
        "{:?}",
        all_proxy.problems()
    );
    let shadowed = with_env(
        pooled(),
        from_env(&[
            ("HTTPS_PROXY", "http://127.0.0.1:8080"),
            ("ALL_PROXY", AT_TCR),
        ]),
    );
    assert!(!names_tcr(&shadowed), "{:?}", shadowed.problems());
    assert_eq!(
        with_env(pooled(), from_env(&[("HTTPS_PROXY", AT_TCR)])).problems()[0],
        PROBLEM,
        "the plain case reads the same through the environment"
    );

    // A proxy whose interception is unknown may still intercept codex.
    let predates = with_env(
        not_pooled(
            "predates",
            "the proxy on :3999 (build abc1234) predates Codex",
        ),
        at_tcr.clone(),
    );
    assert_eq!(predates.intercept(), "unknown");
    assert_eq!(predates.problems()[0], PROBLEM);
    assert_eq!(
        with_env(
            not_pooled("unknown-leaf", "the proxy on :3999 reports a leaf"),
            at_tcr
        )
        .intercept(),
        "unknown"
    );

    // A credential in the value is never printed.
    let with_userinfo = format!("http://{}:{}@127.0.0.1:3999", "u", "p");
    let credentialed = with_env(
        pooled(),
        CodexEnv {
            https_proxy: Some(("HTTPS_PROXY".to_string(), with_userinfo)),
            ..CodexEnv::default()
        },
    );
    assert_eq!(
        env_line(&credentialed),
        "codexEnv: HTTPS_PROXY=http://…@127.0.0.1:3999 (points at tcr) · NO_PROXY unset · \
             SSL_CERT_FILE unset"
    );
    let secret = format!("{}:{}", "u", "p");
    for line in credentialed.block_lines() {
        assert!(!line.contains(&secret), "{line}");
    }
    assert!(
        !credentialed.to_json().to_string().contains(&secret),
        "{}",
        credentialed.to_json()
    );
    assert_eq!(
        credentialed.to_json()["env"]["httpsProxy"],
        "http://…@127.0.0.1:3999"
    );
}

/// A Codex account in a reserved group serves nothing, because a Codex request carries no
/// group: one problem per such row, its reserved groups joined, in row order.
#[test]
fn a_codex_problem_names_a_reserved_codex_account() {
    let reserved = |name: &str, groups: &[&str]| CodexRow {
        reserved_groups: groups.iter().map(ToString::to_string).collect(),
        ..codex_row(name, Some(GateReason::Reserved))
    };
    let report = CodexReport {
        rows: vec![
            reserved("kim@example.com", &["research"]),
            codex_row("bob@example.com/codex", Some(GateReason::Ok)),
            reserved("lee@example.com/codex", &["research", "orchard"]),
        ],
        ..codex_report(pooled())
    };
    assert_eq!(
        report.problems(),
        [
            "kim@example.com is reserved for research, which no Codex request can ask for, \
                 so it serves nothing",
            "lee@example.com/codex is reserved for research, orchard, which no Codex \
                 request can ask for, so it serves nothing",
        ]
    );
    let offline = CodexReport {
        rows: vec![codex_row("kim@example.com", None)],
        ..codex_report(pooled())
    };
    assert!(offline.problems().is_empty(), "{:?}", offline.problems());
}

/// The pooled verdict counts its requests in the right number: one Codex request reads
/// `1 Codex request`, and any other count, none included, reads `requests`.
///
/// Watched red: with the noun always plural, one request reads `1 Codex requests`.
#[test]
fn the_pooled_verdict_says_one_request_in_the_singular() {
    let verdict = |requests: u64| {
        CodexReport {
            requests_10m: Some(requests),
            ..codex_report(pooled())
        }
        .verdict()
    };
    assert_eq!(
        verdict(1),
        "codex is pooled: `tcr run --codex` routes chatgpt.com through this proxy, which \
             served 1 Codex request in the last 10 min"
    );
    for requests in [0, 2] {
        assert!(
            verdict(requests).ends_with(&format!(
                "served {requests} Codex requests in the last 10 min"
            )),
            "{}",
            verdict(requests)
        );
    }
}

/// The Codex verdict reuses the doctor's exit codes: 0 pooled, 2 when codex would not be
/// pooled and the verdict says why, 3 when no proxy answered or its status could not be
/// read.
#[test]
fn the_codex_exit_codes_reuse_the_meanings() {
    let no_codex_account = crate::codex_route::no_codex_account_reason(3999, "chatgpt.com");
    assert_eq!(
        no_codex_account,
        "the proxy on :3999 holds no Codex account, so it blind-tunnels chatgpt.com and codex \
             keeps its own login — add one with `tcr login --codex`"
    );
    let mitm_off = "the proxy on :3999 is running without MITM: its TLS material failed to \
                        load at boot — see its log";
    let unreadable = "could not read the running proxy's status (the server did not answer \
                          within 5s)";
    let offline = |route: CodexRoute| CodexReport {
        requests_10m: None,
        traffic: None,
        ..codex_report(route)
    };
    let cases = [
        (
            CodexReport {
                requests_10m: Some(42),
                ..codex_report(pooled())
            },
            "codex is pooled: `tcr run --codex` routes chatgpt.com through this proxy, which \
                 served 42 Codex requests in the last 10 min"
                .to_string(),
            0,
        ),
        (
            codex_report(not_pooled("mitm-off", mitm_off)),
            format!("codex is not pooled: {mitm_off}"),
            2,
        ),
        (
            codex_report(not_pooled("no-codex-account", &no_codex_account)),
            format!("codex is not pooled: {no_codex_account}"),
            2,
        ),
        (
            codex_report(not_pooled(
                "no-ca",
                "no readable CA at /Users/example/.config/tcr-ca.pem",
            )),
            "codex is not pooled: no readable CA at /Users/example/.config/tcr-ca.pem".to_string(),
            2,
        ),
        (
            offline(not_pooled(
                "no-proxy",
                "no proxy answered on 127.0.0.1:3999",
            )),
            "codex is not pooled: no proxy answered on 127.0.0.1:3999".to_string(),
            3,
        ),
        (
            offline(not_pooled("status-unreadable", unreadable)),
            format!("codex is not pooled: {unreadable}"),
            3,
        ),
    ];
    for (report, verdict, exit) in cases {
        assert_eq!(report.verdict(), verdict);
        assert_eq!(report.exit_code(), exit, "{verdict}");
        assert_eq!(report.to_json()["verdict"], verdict.as_str());
        assert_eq!(report.to_json()["exitCode"], exit);
    }
}

/// Beside a Codex block, Claude's served verdict says it counts Claude requests, in the
/// text and the JSON alike; a fleet with no Codex block keeps the words it always had. A
/// mixed fleet's exit code is Claude's; with no Claude account, or with `--codex`, it is
/// Codex's.
#[test]
fn the_claude_verdict_names_claude_only_beside_codex() {
    let claude = report_for("http://127.0.0.1:3456", BaseUrlSource::ProcessEnv, Some(3));
    let diagnosis = |claude: Report, codex: Option<CodexReport>| Diagnosis {
        claude,
        codex,
        holds_anthropic: true,
        codex_only: false,
    };
    let last_line = |diagnosis: &Diagnosis| {
        diagnosis
            .render_text()
            .lines()
            .last()
            .expect("a last line")
            .to_string()
    };

    let alone = diagnosis(claude.clone(), None);
    let today = "verdict: Claude is routed to http://127.0.0.1:3456 (ANTHROPIC_BASE_URL); \
                     this proxy has served 3 requests";
    assert_eq!(last_line(&alone), today);
    assert_eq!(alone.to_json()["verdict"], today);
    assert_eq!(alone.render_text(), claude.render_text());
    assert_eq!(alone.to_json(), claude.to_json());

    let mixed = diagnosis(claude.clone(), Some(codex_report(pooled())));
    let beside = "verdict: Claude is routed to http://127.0.0.1:3456 (ANTHROPIC_BASE_URL); \
                      this proxy has served 3 Claude requests";
    assert_eq!(last_line(&mixed), beside);
    assert_eq!(mixed.to_json()["verdict"], beside);
    let one = diagnosis(
        report_for("http://127.0.0.1:3456", BaseUrlSource::ProcessEnv, Some(1)),
        Some(codex_report(pooled())),
    );
    assert!(
        last_line(&one).ends_with("this proxy has served 1 Claude request"),
        "{}",
        last_line(&one)
    );

    for other_arm in [
        report_for(
            "https://gateway.example.com",
            BaseUrlSource::ProcessEnv,
            None,
        ),
        report_for("http://127.0.0.1:3456", BaseUrlSource::ProcessEnv, None),
    ] {
        let verdict = other_arm.verdict();
        let mixed = diagnosis(other_arm.clone(), Some(codex_report(pooled())));
        assert_eq!(last_line(&mixed), verdict);
        assert_eq!(mixed.to_json()["verdict"], verdict.as_str());
        assert_eq!(last_line(&diagnosis(other_arm, None)), verdict);
    }

    // The exit code, per shape: Claude's exits 0, Codex's 3.
    assert_eq!(claude.exit_code(), 0);
    let codex_three = CodexReport {
        requests_10m: None,
        traffic: None,
        ..codex_report(not_pooled(
            "no-proxy",
            "no proxy answered on 127.0.0.1:3999",
        ))
    };
    assert_eq!(codex_three.exit_code(), 3);
    let shape = |codex: Option<CodexReport>, holds_anthropic: bool, codex_only: bool| {
        Diagnosis {
            claude: claude.clone(),
            codex,
            holds_anthropic,
            codex_only,
        }
        .exit_code()
    };
    assert_eq!(shape(None, true, false), 0, "no Codex block");
    assert_eq!(
        shape(Some(codex_three.clone()), true, false),
        0,
        "mixed: Claude's, always"
    );
    assert_eq!(
        shape(Some(codex_three.clone()), false, false),
        3,
        "no Claude account: Codex's"
    );
    assert_eq!(shape(Some(codex_three.clone()), true, true), 3, "--codex");
    assert_eq!(shape(Some(codex_three), false, true), 3, "--codex");
}

/// The shell route's served verdict says its count is Claude's beside a Codex block, as
/// the base-URL arm does: `requests10m` counts Claude sessions alone, and a reader who also
/// sees `codexRequests10m` would take "3 requests" for every request the proxy served.
/// With no Codex block it keeps its own words, and with no proxy answering it names no
/// count to qualify.
#[test]
fn the_shell_routes_verdict_names_claude_requests_beside_codex() {
    let shell = |served| {
        let mut report = report_for(DEFAULT_BASE_URL, BaseUrlSource::Default, served);
        report.shell_route = Some(PathBuf::from("/home/alice/.zshrc"));
        report
    };
    let head = "verdict: claude typed in a terminal goes through tcr run (the tcr block in \
                    /home/alice/.zshrc); apps that start claude themselves use \
                    https://api.anthropic.com (the default); ";

    let report = shell(Some(3));
    assert_eq!(
        report.verdict(),
        format!("{head}this proxy has served 3 requests")
    );
    let beside = format!("{head}this proxy has served 3 Claude requests");
    assert_eq!(report.verdict_beside_codex(), beside);
    let mixed = Diagnosis {
        claude: report,
        codex: Some(codex_report(pooled())),
        holds_anthropic: true,
        codex_only: false,
    };
    assert_eq!(
        mixed.render_text().lines().last(),
        Some(beside.as_str()),
        "{}",
        mixed.render_text()
    );
    assert_eq!(mixed.to_json()["verdict"], beside.as_str());
    assert_eq!(mixed.exit_code(), 0, "the shell route is Claude's route");

    assert_eq!(
        shell(Some(1)).verdict_beside_codex(),
        format!("{head}this proxy has served 1 Claude request")
    );
    let silent = shell(None);
    assert_eq!(silent.verdict_beside_codex(), silent.verdict());
}

/// A config's unknown keys ride the Claude half: after `requests10m:` and Claude's
/// `problem:` lines, before `codexAccounts:`, and in the top-level `unknownKeys`. With
/// `--codex` there is no Claude half, and they come right before the verdict and in the
/// document's own `unknownKeys`: a misspelled `codexUpstream` matters most there.
#[test]
fn unknown_config_keys_ride_the_claude_half_in_every_shape() {
    const LINE: &str = "unknownKey: codexUpstrem (did you mean codexUpstream?)";
    let claude = report_for("http://127.0.0.1:3456", BaseUrlSource::ProcessEnv, Some(3))
        .with_config_keys(&config_from(
            r#"{"accounts":[],"codexUpstrem":"https://chatgpt.com"}"#,
        ));
    let keys = serde_json::json!([{"key": "codexUpstrem", "nearest": "codexUpstream"}]);
    let diagnosis = |codex: bool, holds_anthropic: bool, codex_only: bool| Diagnosis {
        claude: claude.clone(),
        codex: codex.then(|| codex_report(pooled())),
        holds_anthropic,
        codex_only,
    };
    let position = |lines: &[&str], prefix: &str| {
        lines
            .iter()
            .position(|line| line.starts_with(prefix))
            .unwrap_or_else(|| panic!("no line starts {prefix:?}: {lines:#?}"))
    };

    let alone = diagnosis(false, true, false);
    let text = alone.render_text();
    assert_eq!(text, claude.render_text());
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[lines.len() - 2], LINE);

    for (label, holds_anthropic) in [("mixed", true), ("Codex-only", false)] {
        let shape = diagnosis(true, holds_anthropic, false);
        let text = shape.render_text();
        let lines: Vec<&str> = text.lines().collect();
        let unknown = position(&lines, "unknownKey: ");
        assert_eq!(lines[unknown], LINE, "{label}");
        assert!(
            position(&lines, "requests10m: ") < unknown
                && unknown < position(&lines, "codexAccounts: "),
            "{label}: {text}"
        );
        assert_eq!(shape.to_json()["unknownKeys"], keys, "{label}");
        assert!(shape.to_json()["codex"].is_object(), "{label}");
    }

    let codex_only = diagnosis(true, true, true);
    let text = codex_only.render_text();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[lines.len() - 2], LINE, "{text}");
    assert!(lines[lines.len() - 1].starts_with("verdict: "), "{text}");
    assert_eq!(codex_only.to_json()["unknownKeys"], keys);
    assert!(codex_only.to_json().get("baseUrl").is_none());
}

/// `codexRequests10m` counts Codex sessions only, the sessions `requests10m` leaves out.
#[test]
fn codex_requests_in_the_window_count_codex_sessions_only() {
    let mut claude = vec![0u16; 30];
    claude[29] = 5;
    let mut codex_traffic = vec![0u16; 30];
    codex_traffic[29] = 7;
    codex_traffic[10] = 40;
    let mut codex = session(codex_traffic);
    codex.provider = Some("codex".to_string());
    let payload = payload_with(vec![session(claude), codex]);
    assert_eq!(
        codex_requests_in_window(&payload, REQUEST_WINDOW_MINUTES),
        7
    );
    assert_eq!(requests_in_window(&payload, REQUEST_WINDOW_MINUTES), 5);
}
