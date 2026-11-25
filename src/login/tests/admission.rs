use super::*;

/// The config at `path`, read once, as `admit_login` hands it to the guard.
fn cfg(path: &Path) -> Config {
    load_or_default(path).expect("test config loads")
}

#[test]
fn login_guard_refusal_message_carries_pid_and_stop_login_restart_sequence() {
    let msg = expect_refuse(login_guard_refusal(
        incumbent(singleton::ProxyKind::Tcr),
        false,
        3456,
        false,
    ));
    // Actionable + greppable: names the port, the PID, and the escape hatch.
    assert!(msg.contains("3456"), "names the port: {msg}");
    assert!(msg.contains("4242"), "names the pid: {msg}");
    assert!(msg.contains("--force"), "names the escape hatch: {msg}");
    assert_eq!(
        msg.lines().count(),
        1,
        "the refusal is a single line: {msg}"
    );
    // The recovery is the ORDERED stop → login → restart sequence: stop names the
    // exact PID, then `tcr login`, then restart `tcr server`.
    let stop = msg.find("kill 4242").expect("stop step names the pid");
    let login = msg.find("tcr login").expect("login step present");
    let restart = msg.find("restart").expect("restart step present");
    assert!(
        stop < login && login < restart,
        "stop → login → restart, in order: {msg}"
    );
    assert!(
        msg.contains("tcr server"),
        "restart targets the server: {msg}"
    );
}

#[test]
fn login_guard_allows_with_force_or_no_server() {
    // --force is the deliberate escape hatch even when a server is live —
    // it takes the file path regardless of whether the route is there.
    assert_eq!(
        login_guard_refusal(incumbent(singleton::ProxyKind::Tcr), false, 3456, true),
        LoginRoute::File
    );
    // No server on the port → nothing to refuse.
    assert_eq!(
        login_guard_refusal(None, false, 3456, false),
        LoginRoute::File
    );
}

/// THE BRIDGE ITEM C FIX: `--force` overrides only the `Refuse` outcome,
/// never a confirmed-safe `Live` route. The pre-fix code checked `force`
/// BEFORE `has_add_route`, so `--force` took the file path even when the
/// live route was confirmed and safe — the exact bug this unit exists to
/// remove, self-inflicted by the escape hatch. Nobody wants the file
/// write *because* it is a file write; they want the login not refused.
#[test]
fn login_guard_prefers_live_over_force_when_the_route_is_confirmed() {
    assert_eq!(
        login_guard_refusal(incumbent(singleton::ProxyKind::Tcr), true, 3456, true),
        LoginRoute::Live,
        "--force must not choose the file write when the live route IS available and safe"
    );
}

/// THE NEW OUTCOME. A live proxy that HAS the add route is no longer
/// refused at all — the whole point of this unit.
#[test]
fn login_guard_proceeds_live_when_the_incumbent_has_the_add_route() {
    assert_eq!(
        login_guard_refusal(incumbent(singleton::ProxyKind::Tcr), true, 3456, false),
        LoginRoute::Live
    );
    // Even an embedded host, whose absent-route refusal has the special
    // "quit, don't kill" wording — once the route is confirmed there is
    // nothing left to refuse, embedded or not.
    assert_eq!(
        login_guard_refusal(
            incumbent(singleton::ProxyKind::TcrEmbedded),
            true,
            3456,
            false
        ),
        LoginRoute::Live
    );
}

/// THE CASE THE PROBE-FIRST ORDERING EXISTS FOR: the HTTP probe confirmed
/// the route, but pid/argv/owner-file detection found NOTHING on the port
/// (`incumbent: None`) — a host application `singleton::classify_proxy_server`
/// was never taught to recognize, or a stale/unreadable owner file. The
/// probe is first-hand evidence and must win on its own; requiring
/// `incumbent` to agree would silently fall back to `File` — the
/// whole-file clobber this unit exists to remove — the moment that
/// unrelated detection heuristic misses.
#[test]
fn login_guard_proceeds_live_on_a_confirmed_route_even_when_incumbent_detection_missed_it() {
    assert_eq!(
        login_guard_refusal(None, true, 3456, false),
        LoginRoute::Live
    );
}

/// THE ADVICE MUST BE SURVIVABLE. `live_proxy_server` can now name the pid of
/// the host application that serves the proxy in-process, and telling the
/// operator to `kill` it is the one action `singleton` is hardened against
/// twice over: AppKit installs no SIGTERM handler, so the app dies without
/// `applicationWillTerminate` and the final session→account pin write is lost.
/// A guard that refuses correctly and then advises the destructive fix is worse
/// than no guard, because the operator does what the tool says.
#[test]
fn the_login_guard_never_tells_an_operator_to_kill_a_host_application() {
    let msg = expect_refuse(login_guard_refusal(
        incumbent(singleton::ProxyKind::TcrEmbedded),
        false,
        3456,
        false,
    ));
    assert!(
        !msg.contains("kill 4242"),
        "must not advise signalling the host application: {msg}"
    );
    assert!(
        !msg.contains("Ctrl-C"),
        "the host application is not a foreground process to interrupt: {msg}"
    );
    assert!(
        msg.contains("quit the host application"),
        "must name the one action that stops an embedded proxy safely: {msg}"
    );
    // Everything the CLI refusal is load-bearing for still holds: one line,
    // names the port, the pid and the escape hatch.
    assert!(
        msg.contains("3456") && msg.contains("4242") && msg.contains("--force"),
        "{msg}"
    );
    assert_eq!(
        msg.lines().count(),
        1,
        "the refusal is a single line: {msg}"
    );

    // And the control: a plain CLI peer, which the operator CAN signal, still
    // gets the kill instruction — so the assertions above are about the kind,
    // not about the message having been softened for everyone.
    let cli = expect_refuse(login_guard_refusal(
        incumbent(singleton::ProxyKind::Tcr),
        false,
        3456,
        false,
    ));
    assert!(cli.contains("kill 4242"), "{cli}");
}

#[test]
fn login_refuses_over_proxy_command_lines_but_allows_an_unrelated_holder() {
    use crate::singleton::is_proxy_server;
    // The real chain: a port holder blocks login IFF its argv is a
    // recognized tcr *server*. Compose the pure classifier with the
    // pure guard decision — refuse / allow. Argv, not a joined
    // command line: `singleton::classify_proxy_server` takes pre-split argv
    // (as `sysinfo::Process::cmd()` returns it) so a space in an executable's
    // path can never be re-split into the wrong tokens.
    let refuses = |cmd: &[&str]| {
        let argv: Vec<String> = cmd.iter().map(|s| (*s).to_string()).collect();
        let server = singleton::classify_proxy_server(&argv)
            .map(|kind| singleton::Incumbent { pid: 4242, kind });
        assert_eq!(
            server.is_some(),
            is_proxy_server(&argv),
            "the bool and the classifying form must agree: {cmd:?}"
        );
        matches!(
            login_guard_refusal(server, false, 3456, false),
            LoginRoute::Refuse(_)
        )
    };
    assert!(
        refuses(&["/opt/tcr/target/release/tcr", "server"]),
        "a live tcr server must block login"
    );
    assert!(
        !refuses(&["python3", "-m", "http.server", "3456"]),
        "an unrelated process on the port must NOT block login"
    );
}

// --- probe_add_capability -----------------------------------------------

#[tokio::test]
async fn probe_add_capability_reads_a_stamped_400_as_present() {
    let (listener, port) = crate::test_support::loopback::listener().await;
    let seed = format!(r#"{{ "proxy": {{ "port": {port} }} }}"#);
    serve_live_proxy(listener, &seed, None);
    let config: Config = serde_json::from_str(&seed).unwrap();

    assert_eq!(probe_add_capability(&config).await, AddCapability::Present);
}

/// Bridge item D: an "older tcr" — something answers, but not on this
/// route at all (no `ENDPOINT_HEADER`, unlike every response this crate's
/// own router produces, even its 404s and 405s) — is first-hand evidence
/// a live process holds the port. It must NOT collapse into `Absent`
/// (renamed from this test's old expectation): folding it in there is
/// exactly what let the login guard silently choose `File` beside a real,
/// answering-but-old server whenever pid detection missed it.
#[tokio::test]
async fn probe_add_capability_unstamped_404_is_unusable_not_absent() {
    let port = empty_router_port();
    let config: Config =
        serde_json::from_str(&format!(r#"{{ "proxy": {{ "port": {port} }} }}"#)).unwrap();
    assert!(matches!(
        probe_add_capability(&config).await,
        AddCapability::Unusable(_)
    ));
}

/// The genuinely-absent case, for contrast with the test above: nothing
/// is bound to the port at all, so the connection is refused outright.
/// This is the ONLY shape that may still resolve to `LoginRoute::File`
/// without `--force` and without `incumbent` separately confirming a
/// server.
#[tokio::test]
async fn probe_add_capability_connection_refused_is_absent() {
    let dead_port = DEAD_PORT;
    let config: Config =
        serde_json::from_str(&format!(r#"{{ "proxy": {{ "port": {dead_port} }} }}"#)).unwrap();
    assert_eq!(probe_add_capability(&config).await, AddCapability::Absent);
}

/// The WEDGED shape `cli::probe_incumbent`'s own tests document as
/// measured and real (`a_listener_that_never_answers_is_silent_not_answering`):
/// bound, the connect succeeds off the kernel backlog, and nothing is
/// ever written back. Costs the probe's own 5s deadline by construction.
#[tokio::test]
async fn probe_add_capability_wedged_port_is_unusable() {
    let port = wedged_port();
    let config: Config =
        serde_json::from_str(&format!(r#"{{ "proxy": {{ "port": {port} }} }}"#)).unwrap();

    assert!(matches!(
        probe_add_capability(&config).await,
        AddCapability::Unusable(_)
    ));
}

#[tokio::test]
async fn probe_add_capability_wrong_api_key_is_unauthorized_not_absent() {
    let port = live_proxy_requiring_the_api_key("correct-key").await;

    // The caller's config carries the WRONG key.
    let client_config: Config = serde_json::from_str(&format!(
        r#"{{ "proxy": {{ "port": {port}, "apiKey": "wrong-key" }} }}"#
    ))
    .unwrap();
    assert_eq!(
        probe_add_capability(&client_config).await,
        AddCapability::Unauthorized,
        "a rejected api-key must surface as itself, never read as 'no route'"
    );
}

// --- admit_login_against ----------------------------------------------------------

/// THE INTEGRATION-LEVEL VERSION of
/// `login_guard_proceeds_live_on_a_confirmed_route_even_when_incumbent_detection_missed_it`:
/// a real live server answering the real route, called with `incumbent:
/// None` (simulating a pid/argv/owner-file detection MISS — the TcrBar
/// case, where `argv[0]` ends in `/TcrBar` and
/// `singleton::classify_proxy_server` recognizes nothing). `admit_login_against`
/// must still probe and proceed live — it must never gate the probe
/// itself on `incumbent.is_some()`, which is exactly the shape of bug
/// this test catches: skipping the probe here would fall through to
/// `WriteRoute::File` beside a real, answering server.
#[tokio::test]
async fn admit_login_proceeds_live_even_when_incumbent_detection_missed_a_real_server() {
    let (path, port) = a_live_proxy_and_its_config_file("missed-incumbent").await;

    let route = admit_login_against(&path, &cfg(&path), None, port, false, LoginCommand::Browser)
        .await
        .expect("a confirmed route must not error")
        .route;
    assert_eq!(
        route,
        WriteRoute::Live,
        "the probe must decide this on its own, independent of incumbent detection"
    );
    std::fs::remove_file(&path).ok();
}

/// Bridge item C, at the `admit_login_against` level: `--force` must still probe
/// and still prefer `Live` when a real server confirms the route — never
/// skip straight to `File` the way the pre-fix early-return did.
#[tokio::test]
async fn admit_login_prefers_live_over_force_when_a_real_server_confirms_the_route() {
    let (path, port) = a_live_proxy_and_its_config_file("force-live").await;

    let route = admit_login_against(&path, &cfg(&path), None, port, true, LoginCommand::Browser)
        .await
        .expect("a confirmed route under --force must not error")
        .route;
    assert_eq!(
        route,
        WriteRoute::Live,
        "--force must still probe and prefer Live when the route is confirmed"
    );
    std::fs::remove_file(&path).ok();
}

/// Bridge item D, at the `admit_login_against` level: a wedged port — something
/// answers the TCP connect and then never responds — must refuse even
/// when `incumbent` is `None` (pid/argv detection missed it entirely, the
/// TcrBar shape). Before this fix, `AddCapability::Absent` covered this
/// case too, and `login_guard_refusal`'s `None => WriteRoute::File` arm
/// let it through with no `--force` needed at all.
#[tokio::test]
async fn admit_login_refuses_a_wedged_port_even_when_incumbent_detection_missed_it() {
    let port = wedged_port();
    let path = std::env::temp_dir().join(format!(
        "tcr-oauth-route-wedged-{}-{port}.json",
        std::process::id()
    ));
    std::fs::write(&path, format!(r#"{{ "proxy": {{ "port": {port} }} }}"#)).unwrap();

    let err = admit_login_against(&path, &cfg(&path), None, port, false, LoginCommand::Browser)
        .await
        .expect_err("a wedged port must refuse, not silently choose File");
    assert!(err.to_string().contains("--force"), "{err}");

    std::fs::remove_file(&path).ok();
}

/// The escape hatch still works past a wedged-port refusal: `--force`
/// resolves to `File` rather than erroring out.
#[tokio::test]
async fn admit_login_force_resolves_to_file_past_a_wedged_port_refusal() {
    let port = wedged_port();
    let path = std::env::temp_dir().join(format!(
        "tcr-oauth-route-wedged-force-{}-{port}.json",
        std::process::id()
    ));
    std::fs::write(&path, format!(r#"{{ "proxy": {{ "port": {port} }} }}"#)).unwrap();

    let route = admit_login_against(&path, &cfg(&path), None, port, true, LoginCommand::Browser)
        .await
        .expect("--force must still resolve past a wedged-port refusal")
        .route;
    assert_eq!(route, WriteRoute::File);

    std::fs::remove_file(&path).ok();
}

/// A refusal comes back as the error, naming the login that was run, and
/// `--force` turns the same refusal into an admitted file route: an
/// `Admitted` is never `Refuse`.
#[tokio::test]
async fn admit_login_bails_on_a_refusal_naming_the_command_and_force_admits_the_file() {
    let port = DEAD_PORT;
    let path = std::env::temp_dir().join(format!(
        "tcr-oauth-admit-refusal-{}-{port}.json",
        std::process::id()
    ));
    std::fs::write(&path, format!(r#"{{ "proxy": {{ "port": {port} }} }}"#)).unwrap();

    let err = admit_login_against(
        &path,
        &cfg(&path),
        incumbent(singleton::ProxyKind::Tcr),
        port,
        false,
        LoginCommand::FromClaudeCode,
    )
    .await
    .expect_err("an incumbent with no add route must refuse");
    assert!(
        err.to_string().contains(LOGIN_FROM_CLAUDE_CODE_COMMAND),
        "{err}"
    );

    let admitted = admit_login_against(
        &path,
        &cfg(&path),
        incumbent(singleton::ProxyKind::Tcr),
        port,
        true,
        LoginCommand::FromClaudeCode,
    )
    .await
    .expect("--force admits the file route past that refusal");
    assert_eq!(admitted.route, WriteRoute::File);
    assert_eq!(admitted.pid, Some(4242));
    assert!(admitted.require_codex(LoginCommand::Codex).is_ok());

    std::fs::remove_file(&path).ok();
}

/// THE OVERRULE: `--force` is the escape hatch past a wedged-port
/// refusal (above), but it must NOT be one past `AddCapability::
/// Unauthorized`. A rejected api-key is positive evidence the proxy is
/// alive and healthy — the worst-informed moment to fall back to a
/// whole-file write beside it — so `admit_login_against` must keep refusing even
/// under `--force`, and nothing along the way may touch the config file.
#[tokio::test]
async fn admit_login_refuses_unauthorized_even_under_force() {
    let port = live_proxy_requiring_the_api_key("correct-key").await;

    // The file `admit_login_against` reads carries the WRONG key.
    let path = std::env::temp_dir().join(format!(
        "tcr-oauth-route-unauthorized-force-{}-{port}.json",
        std::process::id()
    ));
    let seed = format!(r#"{{ "proxy": {{ "port": {port}, "apiKey": "wrong-key" }} }}"#);
    std::fs::write(&path, &seed).unwrap();
    let before = std::fs::read_to_string(&path).unwrap();

    let err = admit_login_against(&path, &cfg(&path), None, port, true, LoginCommand::Browser)
        .await
        .expect_err("--force must not rescue a rejected api-key");
    assert!(err.to_string().contains("api-key"), "{err}");
    assert!(
        err.to_string().contains("even under --force"),
        "the message must say --force does not apply here: {err}"
    );

    assert_eq!(
        before,
        std::fs::read_to_string(&path).unwrap(),
        "the file must be byte-identical — --force must not trigger a write here"
    );
    std::fs::remove_file(&path).ok();
}
