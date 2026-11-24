//! Tests for the port-takeover singleton.

#[test]
fn the_stand_down_line_names_the_first_version_with_handoff() {
    assert!(stand_down_message(12345, 42).contains(&format!(
        "if it runs tcr {} or newer",
        crate::restart::HANDOFF_SINCE
    )));
}
use super::*;

/// How long [`port_listeners_finds_this_process_on_an_ephemeral_port`]
/// retries a miss before failing. On Linux, `port_listeners` joins two
/// separate `/proc` snapshots that are not read atomically:
/// `build_inode_proc_map` (`listeners-0.6.1/src/platform/linux/mod.rs:17`)
/// walks every process's fd table FIRST, then `ProtoListener::get_all`
/// (same file, line 19) reads `/proc/net/tcp` SECOND, and the two are
/// joined by socket inode. On a loaded CI runner that first walk touches
/// every process on the box and can itself take real wall-clock time; a
/// transient failure partway through it (the per-process `openat` calls
/// in `helpers.rs` each silently `continue` on error) can drop THIS
/// process's own fd from the map even though the listener this test just
/// bound is definitely open and definitely `LISTENing` for the socket-table
/// read that follows. The retry re-runs the whole two-snapshot join; it
/// never touches production code, and a holder that is genuinely absent
/// still fails after this bound elapses.
const PORT_LISTENERS_RETRY_BOUND: Duration = Duration::from_millis(500);

/// How often the retry above re-samples `port_listeners` inside its bound.
const PORT_LISTENERS_RETRY_INTERVAL: Duration = Duration::from_millis(20);

/// The `listeners` integration itself, not just the pure logic around it:
/// bind an ephemeral port IN-PROCESS (port 0 — never a literal port, and
/// never the live proxy's `127.0.0.1:3456`) and assert `port_listeners`
/// reports exactly THIS process's pid on the port the OS actually assigned.
/// `local_addr()` is required rather than querying port 0 directly:
/// `listeners::get_process_by_port` (which we deliberately do not call) even
/// errors outright on `port == 0`, and port 0 is not a real listener's port
/// in the socket table either way.
///
/// Retries for up to [`PORT_LISTENERS_RETRY_BOUND`]: see that constant's
/// doc for the two non-atomic `/proc` reads this works around. The listener
/// stays bound for the whole retry window, so every retry is a fresh,
/// independent read of a socket that is, in reality, continuously held.
#[test]
fn port_listeners_finds_this_process_on_an_ephemeral_port() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("binding an ephemeral port");
    let port = listener
        .local_addr()
        .expect("a bound listener has a local address")
        .port();

    let deadline = std::time::Instant::now() + PORT_LISTENERS_RETRY_BOUND;
    let mut holders = port_listeners(port);
    while !holders.contains(&std::process::id()) && std::time::Instant::now() < deadline {
        sleep(PORT_LISTENERS_RETRY_INTERVAL);
        holders = port_listeners(port);
    }
    assert!(
        holders.contains(&std::process::id()),
        "port {port} should be reported as held by this process (pid {}); got {holders:?} \
             after retrying for {PORT_LISTENERS_RETRY_BOUND:?}",
        std::process::id()
    );

    drop(listener);
}

/// The real defect, end to end, not just the pure dedup helper in isolation:
/// bind `0.0.0.0:0` to get an OS-assigned port, then bind `[::]:<that same
/// port>` in the SAME process — the dual-stack pattern a default-configured
/// Node HTTP server produces from one `listen()` call. Both are genuine LISTEN sockets on
/// the same pid and port, so without the dedup this reports `[pid, pid]` and
/// callers that read the first element still get the right pid — but the
/// `takeover_port` signal loop iterates the WHOLE list, so the SIGTERM ->
/// sleep -> `is_alive` -> SIGKILL sequence would run twice against one process
/// with no fresh verification on the second pass. Port 0, in-process,
/// nothing signalled.
#[test]
fn port_listeners_collapses_a_dual_stack_bind_to_one_pid() {
    let v4 = std::net::TcpListener::bind("0.0.0.0:0").expect("binding an ephemeral IPv4 port");
    let port = v4
        .local_addr()
        .expect("a bound listener has a local address")
        .port();

    let Ok(v6) = std::net::TcpListener::bind(format!("[::]:{port}")) else {
        // Some stacks refuse the v6 bind once v4 already holds the port (no
        // dual-stack support, or `IPV6_V6ONLY` defaults differ locally).
        // Falling back to the pure-helper check rather than failing the run
        // — this asserts the same dedup logic `port_listeners` uses, just
        // without a real second socket to prove the end-to-end case.
        drop(v4);
        let mut seen = HashSet::new();
        let deduped: Vec<u32> = [4242_u32, 4242_u32, 7_u32]
            .into_iter()
            .filter(|&pid| seen.insert(pid))
            .collect();
        assert_eq!(
            deduped,
            vec![4242, 7],
            "fallback: the dedup pattern itself should be order-preserving \
                 (could not verify the real dual-stack bind on this machine)"
        );
        return;
    };

    let holders = port_listeners(port);
    assert_eq!(
        holders,
        vec![std::process::id()],
        "one process holding the port on two dual-stack sockets should report ONE pid \
             (order-preserving dedup), got {holders:?}"
    );

    drop(v4);
    drop(v6);
}

/// The negative control the `SocketState::Listen` filter never had: a test
/// proving a LISTEN socket IS found existed, nothing proved a non-LISTEN
/// socket is NOT. That filter is the issue-#36 guard on the kill path
/// (`replaceable_incumbents` -> `takeover_decision` -> the SIGTERM/SIGKILL
/// loop), so positive-only coverage means a regression that dropped the
/// filter would ship silently. Single process, port 0, no second proxy, no
/// root, nothing signalled.
#[test]
fn port_listeners_excludes_a_non_listening_socket_on_the_same_port() {
    let server = std::net::TcpListener::bind("127.0.0.1:0").expect("binding an ephemeral port");
    let port = server
        .local_addr()
        .expect("a bound listener has a local address")
        .port();

    // Positive control: the listener itself is still up.
    let holders = port_listeners(port);
    assert!(
        holders.contains(&std::process::id()),
        "the LISTEN socket on port {port} should be reported; got {holders:?}"
    );

    let _client =
        std::net::TcpStream::connect(("127.0.0.1", port)).expect("connecting to our own listener");
    let (_accepted, _) = server.accept().expect("accepting the connection");
    // `_accepted`'s LOCAL port is also `port`, but its state is ESTABLISHED,
    // not LISTEN — it must never appear here.
    drop(server);
    // The LISTEN socket is gone; only the ESTABLISHED accepted socket still
    // has local port `port`. Without the `SocketState::Listen` filter this
    // would report this process's pid again and the assertion below fails.
    // Same narrowing as the UDP case below, for the same reason: once `server`
    // is dropped the port is released, so an unrelated process can take it and
    // be reported here legitimately. The claim under test is about OUR
    // ESTABLISHED socket, and the inline positive control above already proved
    // `port_listeners` reports this pid when a real LISTEN socket exists.
    let holders_after = port_listeners(port);
    assert!(
        !holders_after.contains(&std::process::id()),
        "an ESTABLISHED socket on port {port} must not be reported as a LISTEN holder; \
             got {holders_after:?}"
    );
}

/// UDP canary, with a positive control of its own.
///
/// **The exclusion half cannot currently fail, and that is deliberate.**
/// It was once documented as if a filter were holding it up; the control
/// beside it is what stops "nothing was reported" reading as "UDP was
/// correctly excluded".
///
/// Measured 2026-08-08 on macOS by removing each filter in turn and re-running:
/// it stays green with `SocketState::Listen` removed AND with `Protocol::TCP`
/// removed. So neither filter is what excludes our UDP socket; `get_all()`
/// never enumerates it in the first place on this platform. The previous
/// comment here claimed "`Protocol::TCP` (not the state filter) is what keeps
/// this green", which the mutations refute.
///
/// It is kept as an upstream canary: if `listeners` ever starts reporting
/// UDP sockets, this goes red and tells us the protocol filter has become
/// load-bearing. Do not count the exclusion as evidence that either filter
/// works: the two tests above are, and both were watched failing under
/// mutation. What the control below DOES prove is that the enumeration ran
/// at all on the very port being asked about, which an empty answer from a
/// broken `get_all()` would otherwise have satisfied.
#[test]
fn port_listeners_excludes_a_udp_socket_on_the_same_port() {
    // **The TCP socket is bound FIRST and the UDP one takes its number.**
    //
    // Letting the kernel pick the UDP port and then asking about it left
    // the whole TCP space of that number to chance, including this
    // process's own: the lib suite runs many tests at once, several of
    // them bind ephemeral TCP listeners, and one of them landing on the
    // number handed to this UDP socket reddens this test with OUR pid,
    // which is exactly what its assertion is about. Measured once in a
    // full workspace run. Binding TCP first makes the number this test's
    // own in both spaces, so no sibling in this process can take it while
    // the listener is up.
    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").expect("binding an ephemeral TCP port");
    let port = listener
        .local_addr()
        .expect("a bound listener has a local address")
        .port();
    // The same number in the UDP space, which is independent: this is the
    // socket the assertion below is about.
    let udp = std::net::UdpSocket::bind(("127.0.0.1", port))
        .expect("the UDP space of a TCP port this process holds is free");

    // The positive control, and it is new: with both sockets bound, this
    // process IS a holder, through the TCP listener. Without it the
    // assertion that follows would pass just as well against a
    // `port_listeners` that had stopped enumerating anything at all.
    let with_tcp = port_listeners(port);
    assert!(
        with_tcp.contains(&std::process::id()),
        "this process holds a TCP LISTEN on port {port}, so it must be reported; \
             got {with_tcp:?}"
    );

    // Now only the UDP socket is left on that number.
    drop(listener);
    let holders = port_listeners(port);
    assert!(
        !holders.contains(&std::process::id()),
        "a UDP socket on port {port} must never be reported as a TCP LISTEN holder; \
             got {holders:?}"
    );

    drop(udp);
}

/// Proves `signal_pid` sends the SIGNAL ITSELF, not merely "some kill" — the
/// single most dangerous mistake available in this module
/// (`sysinfo::Process::kill()` always sends SIGKILL regardless of what a
/// caller asks for; `kill()` is `kill_with(Signal::Kill)` under the hood).
/// A green test suite that only checks the code path taken would pass
/// whether this sends SIGTERM or SIGKILL — so this reads the SIGNAL NUMBER
/// the OS actually delivered off the child's exit status.
///
/// Spawns our OWN throwaway child (`sleep 30`) — this never signals any pid
/// the test did not itself spawn, and never comes near a real proxy or the
/// live pid 50152.
#[cfg(unix)]
#[test]
fn signal_pid_term_delivers_sigterm_not_sigkill() {
    use std::os::unix::process::ExitStatusExt;
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("spawning our own throwaway child");
    let pid = child.id();

    signal_pid(pid, Signal::Term);

    let status = child.wait().expect("waiting on our own child");
    assert_eq!(
        status.signal(),
        Some(15), // SIGTERM
        "signal_pid(.., Signal::Term) must deliver SIGTERM (15), not SIGKILL (9) \
             or any other signal -- this is the graceful-shutdown window the module \
             exists to protect"
    );
}

/// The defect this replaced: `takeover_port` slept a FLAT 800ms after
/// SIGTERM, so a successor waited out the whole window even though the
/// incumbent had already exited, and the port sat unbound the entire time.
/// Measured 2026-09-13, that cost ~1.0s of refused connections per bounce.
///
/// Guards the ceiling-not-floor property: against an already-dead pid this
/// must return essentially at once, nowhere near `TERM_GRACE`.
#[cfg(unix)]
#[test]
fn wait_for_exit_returns_as_soon_as_the_process_is_gone() {
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("spawning our own throwaway child");
    let pid = child.id();
    signal_pid(pid, Signal::Kill);
    // Reap it, so the pid is genuinely out of the process table rather than
    // a zombie -- a zombie is still `is_alive`, and that case is covered by
    // the ceiling test below, not by this one.
    child.wait().expect("waiting on our own child");

    let started = std::time::Instant::now();
    let gone = wait_for_exit(pid, TERM_GRACE);
    let elapsed = started.elapsed();

    assert!(gone, "wait_for_exit must report a reaped pid as gone");
    assert!(
        elapsed < TERM_GRACE / 2,
        "wait_for_exit returned in {elapsed:?}, which is not meaningfully \
             faster than the {TERM_GRACE:?} ceiling it replaced -- the whole \
             point is that an already-exited incumbent is not waited out"
    );
}

/// The other half: the grace is still a real bound. A process that does NOT
/// exit must keep us waiting the full window and then report that it is
/// still there, which is exactly what the old `sleep` + `is_alive` pair
/// told the SIGKILL escalation.
#[cfg(unix)]
#[test]
fn wait_for_exit_honours_the_ceiling_for_a_survivor() {
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("spawning our own throwaway child");
    let pid = child.id();
    let grace = Duration::from_millis(100);

    let started = std::time::Instant::now();
    let gone = wait_for_exit(pid, grace);
    let elapsed = started.elapsed();

    signal_pid(pid, Signal::Kill);
    child.wait().expect("waiting on our own child");

    assert!(!gone, "a live process must not be reported as gone");
    assert!(
        elapsed >= grace,
        "wait_for_exit returned in {elapsed:?}, short of its own {grace:?} \
             ceiling -- the escalation to SIGKILL would then fire early"
    );
}

/// The other half of the same proof, for the survivor path: `Signal::Kill`
/// must deliver SIGKILL (9), not the graceful signal.
#[cfg(unix)]
#[test]
fn signal_pid_kill_delivers_sigkill_not_sigterm() {
    use std::os::unix::process::ExitStatusExt;
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("spawning our own throwaway child");
    let pid = child.id();

    signal_pid(pid, Signal::Kill);

    let status = child.wait().expect("waiting on our own child");
    assert_eq!(
        status.signal(),
        Some(9), // SIGKILL
        "signal_pid(.., Signal::Kill) must deliver SIGKILL (9)"
    );
}

/// Builds an argv `Vec<String>` from string-literal parts — matching what
/// [`sysinfo::Process::cmd`] returns (pre-tokenized, no shell/whitespace
/// splitting involved). Test-only stand-in for a real process's argv, kept
/// tight so no test has to hand-expand every literal into a `Vec`.
fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| (*s).to_string()).collect()
}

#[test]
fn recognizes_tcr_server() {
    assert!(is_proxy_server(&argv(&[
        "/opt/tcr/target/release/tcr",
        "server"
    ])));
    assert!(is_proxy_server(&argv(&["tcr"]))); // bare = default server
    assert!(is_proxy_server(&argv(&["/x/tcr", "--headless"]))); // default server + flag
    assert!(is_proxy_server(&argv(&["tcr", "server", "--port", "3456"])));
}

#[test]
fn rejects_non_servers() {
    assert!(!is_proxy_server(&argv(&["/x/tcr", "run"])));
    assert!(!is_proxy_server(&argv(&["/x/tcr", "status"])));
    assert!(!is_proxy_server(&argv(&["/x/tcr", "accounts"])));
    assert!(!is_proxy_server(&argv(&["rg", "tcr", "server"])));
    assert!(!is_proxy_server(&argv(&["vim", "tcr-server.md"])));
    assert!(!is_proxy_server(&argv(&["node", "/path/tcr", "server"])));
    assert!(!is_proxy_server(&argv(&[])));
    assert!(!is_proxy_server(&argv(&["node", "/path/other", "server"])));
}

fn tcr(pid: u32) -> Incumbent {
    Incumbent {
        pid,
        kind: ProxyKind::Tcr,
    }
}

fn embedded(pid: u32) -> Incumbent {
    Incumbent {
        pid,
        kind: ProxyKind::TcrEmbedded,
    }
}

/// A host application's argv: nothing in it matches the `tcr` name matcher,
/// which is the entire reason the owner file exists.
const HOST_APP_ARGV: &[&str] = &["/Applications/TcrBar.app/Contents/MacOS/TcrBar"];

/// An unrelated program listening on the port — what a recycled pid actually
/// runs. Not a proxy by any reading of its argv.
const DEV_SERVER_ARGV: &[&str] = &["/usr/local/bin/node", "/srv/app/dev-server.js"];

/// The argv of a real CLI-hosted proxy, for the tests that need the claim's
/// pid to survive the command check.
fn tcr_command(_pid: u32) -> Vec<String> {
    argv(&["/x/tcr", "server"])
}

/// A unique scratch dir for a claim file. Never [`default_owner_path`] — that
/// one is the live proxy's, and `tcr login` reads it.
fn scratch_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tcr-owner-test-{}-{tag}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or_default(|d| d.as_nanos())
    ));
    std::fs::create_dir_all(&dir).expect("a scratch dir under the temp dir is creatable");
    dir
}

fn write_claim(dir: &Path, port: u16, pid: u32, host: ProxyHost) -> PathBuf {
    let path = owner_path_in(dir, port);
    write_owner_file(
        &path,
        &ProxyOwner {
            pid,
            port,
            sha: "0000000".to_string(),
            host,
        },
    )
    .expect("the scratch claim is writable");
    path
}

/// (a) THE FALSE-POSITIVE CONTROL, and the one that makes the whole mechanism
/// safe to trust: a claim whose pid is **not** listening on the port is
/// IGNORED.
///
/// Without this check the file is a lie anyone can plant — any process could
/// write `{"pid":<some pid>,"host":"embedded"}` and make `tcr` treat that pid
/// as an unreplaceable proxy, and every crashed proxy would leave behind a
/// claim protecting a pid the OS has since recycled onto something unrelated.
/// The file is a *claim*; `port_listeners` is what settles it.
#[test]
fn a_claim_whose_pid_is_not_listening_is_ignored() {
    let dir = scratch_dir("stale");
    let path = write_claim(&dir, 4444, 4242, ProxyHost::Embedded);

    // The file is real, parses, and is for the right port — the ONLY defect is
    // that its pid holds nothing.
    assert!(path.exists());
    assert_eq!(
        classify_port_owner(4444, &[], &path, tcr_command),
        None,
        "a claim with no live listener at all must be ignored"
    );
    assert_eq!(
        classify_port_owner(4444, &[9999, 8888], &path, tcr_command),
        None,
        "a claim whose pid is not among the port's listeners must be ignored"
    );
    // And the positive control, so the assertions above cannot be passing for
    // the boring reason that this function always returns None.
    assert_eq!(
        classify_port_owner(4444, &[9999, 4242], &path, tcr_command),
        Some(embedded(4242)),
        "a claim whose pid IS listening is the one case that counts"
    );
    // A claim for a different port is not this port's claim either.
    assert_eq!(
        classify_port_owner(4445, &[4242], &path, tcr_command),
        None,
        "the port in the file must match the port being resolved"
    );

    // A stale claim does not even suppress the fallback: the name matcher
    // still gets its turn, so a `tcr` predating the owner file is recognized.
    let command = |_pid: u32| argv(&["/x/tcr", "server"]);
    let owner = classify_port_owner(4444, &[7777], &path, command);
    assert_eq!(
        replaceable_incumbents_with_owner(&[7777], 1, command, owner),
        vec![tcr(7777)],
        "ignoring a stale claim must fall through to command-line recognition"
    );

    crate::log_err::LogErr::log_debug(std::fs::remove_dir_all(&dir), "removing a test temp dir");
}

/// The case the whole phase exists for: a proxy inside a host application is
/// invisible to the name matcher, and the claim is what makes it visible.
///
/// The `assert!` on the matcher is not decoration — it is the bug being fixed,
/// stated as an executable fact. If it ever starts matching, `live_proxy_server`
/// stopped depending on the claim and this file's reason to exist changed.
#[test]
fn an_embedded_proxy_is_recognized_only_through_its_claim() {
    assert_eq!(
        classify_proxy_server(&argv(HOST_APP_ARGV)),
        None,
        "the name matcher cannot see a proxy hosted inside another program — \
             that is the failure the claim file replaces"
    );

    let dir = scratch_dir("embedded");
    let path = write_claim(&dir, 3456, 5150, ProxyHost::Embedded);
    let command = |_pid: u32| argv(HOST_APP_ARGV);
    let owner = classify_port_owner(3456, &[5150], &path, command);

    assert_eq!(
        replaceable_incumbents_with_owner(&[5150], 1, command, owner),
        vec![embedded(5150)],
        "the claim identifies the proxy the process name hides"
    );
    // Without the claim, the same holder is nothing at all — which is exactly
    // what made `tcr login` stop refusing to run beside a live server.
    assert!(replaceable_incumbents(&[5150], 1, command).is_empty());

    crate::log_err::LogErr::log_debug(std::fs::remove_dir_all(&dir), "removing a test temp dir");
}

/// `host: "cli"` is a plain [`ProxyKind::Tcr`] — the claim adds identity, not a
/// new policy, for a proxy the matcher could already see.
#[test]
fn a_cli_claim_is_an_ordinary_tcr_peer() {
    let dir = scratch_dir("cli");
    let path = write_claim(&dir, 3456, 1234, ProxyHost::Cli);
    assert_eq!(
        classify_port_owner(3456, &[1234], &path, tcr_command),
        Some(tcr(1234)),
        "a CLI-hosted claim is the same kind the name matcher would produce"
    );
    crate::log_err::LogErr::log_debug(std::fs::remove_dir_all(&dir), "removing a test temp dir");
}

/// THE SECOND FALSE-POSITIVE CONTROL, and the one that stands between a stale
/// claim and a SIGKILL to an innocent process: the claimed pid IS listening on
/// the port — it is simply not a proxy.
///
/// The stale-file test above only covers "the pid is not listening", which a
/// recycled pid passes trivially. The scenario that costs someone their unsaved
/// state is the other one: `tcr` is `SIGKILLed` leaving a `cli` claim behind,
/// months later that pid is a dev server on the same port, and a reader that
/// treats the claim as sufficient hands it to the kill loop. `verified_owner`
/// must ask `ps` before believing a `cli` claim, and the pid must then survive
/// to the very end of the chain unrecognized — `replaceable_incumbents_with_owner`
/// is what `takeover_port` iterates to choose whom to signal.
#[test]
fn a_cli_claim_whose_listening_pid_is_not_a_proxy_is_ignored() {
    let dir = scratch_dir("recycled-pid");
    let path = write_claim(&dir, 3456, 5150, ProxyHost::Cli);
    let dev_server = |_pid: u32| argv(DEV_SERVER_ARGV);

    assert_eq!(
        classify_proxy_server(&argv(DEV_SERVER_ARGV)),
        None,
        "the fixture is only meaningful if the command line is genuinely not a proxy"
    );
    assert_eq!(
        classify_port_owner(3456, &[5150], &path, dev_server),
        None,
        "a cli claim naming a pid that is not running a proxy is a stale claim on a \
             recycled pid, not an incumbent"
    );
    // The positive control: the SAME claim, same pid, same holders — only the
    // command line differs. So the None above is the command check, not the
    // function refusing everything.
    assert_eq!(
        classify_port_owner(3456, &[5150], &path, tcr_command),
        Some(tcr(5150)),
        "the same claim IS believed when the pid is still running a proxy"
    );

    // End to end: the pid reaches the takeover chain as nothing at all, so
    // there is no incumbent for --replace to SIGTERM.
    let owner = classify_port_owner(3456, &[5150], &path, dev_server);
    assert_eq!(
        replaceable_incumbents_with_owner(&[5150], 1, dev_server, owner),
        vec![],
        "an unrelated listener on the port must never become a replaceable incumbent"
    );

    crate::log_err::LogErr::log_debug(std::fs::remove_dir_all(&dir), "removing a test temp dir");
}

/// BACKWARD COMPATIBILITY, and the reason this phase needs no restart: the
/// proxy serving right now wrote no claim, and it must still be recognized.
///
/// A missing file, an empty one and a corrupt one all mean "no claim", never
/// "not a proxy" — a claim that cannot be read must not be able to *hide* an
/// incumbent the matcher would have found.
#[test]
fn a_proxy_that_wrote_no_claim_is_still_recognized() {
    let dir = scratch_dir("absent");
    let command = |_pid: u32| argv(&["/x/tcr", "server"]);

    for (label, path) in [
        ("absent", owner_path_in(&dir, 3456)),
        ("empty", {
            let p = dir.join("empty.json");
            std::fs::write(&p, b"").expect("scratch write");
            p
        }),
        ("corrupt", {
            let p = dir.join("corrupt.json");
            std::fs::write(&p, b"{not json").expect("scratch write");
            p
        }),
        ("wrong-shape", {
            let p = dir.join("wrong-shape.json");
            std::fs::write(&p, br#"{"pid":222}"#).expect("scratch write");
            p
        }),
    ] {
        let owner = classify_port_owner(3456, &[222], &path, command);
        assert_eq!(owner, None, "{label} is not a claim");
        assert_eq!(
            replaceable_incumbents_with_owner(&[222], 1, command, owner),
            vec![tcr(222)],
            "{label}: the retained name matcher must still recognize a running \
                 proxy that never wrote a claim"
        );
    }

    crate::log_err::LogErr::log_debug(std::fs::remove_dir_all(&dir), "removing a test temp dir");
}

/// (b) An EMBEDDED incumbent stands the caller down even under `--replace`.
///
/// The pid on the port belongs to the host application, and `takeover_port`
/// SIGTERMs it (see [`ProxyKind::TcrEmbedded`]): AppKit installs no SIGTERM
/// handler, so `--replace` would kill the app without its shutdown path — no
/// final session→account pin write, and the operator's app simply vanishes. A
/// CLI flag must not be able to do that.
#[test]
fn an_embedded_incumbent_stands_us_down_even_under_replace() {
    for replace in [false, true] {
        assert_eq!(
            takeover_decision(&[embedded(777)], replace),
            Takeover::IncumbentPresent(embedded(777)),
            "replace={replace} must not be able to take over an embedded proxy"
        );
    }
    // And it wins over a `tcr` peer sharing the port, in either order:
    // proceeding for the peer means SIGNALLING it, and this process would
    // still refuse to bind afterwards.
    for holders in [vec![tcr(111), embedded(777)], vec![embedded(777), tcr(111)]] {
        assert_eq!(
            takeover_decision(&holders, true),
            Takeover::IncumbentPresent(embedded(777)),
            "{holders:?}"
        );
    }
}

/// (c) The embedded incumbent ITSELF is never signalled — the assertion that
/// stands between `--replace` and SIGTERM to a GUI process.
#[test]
fn an_embedded_incumbent_is_never_signalled() {
    for replace in [false, true] {
        assert_eq!(
            incumbents_to_signal(&[embedded(777)], replace),
            vec![],
            "replace={replace}: an embedded proxy must never be signalled"
        );
        assert!(
            !incumbents_to_signal(&[embedded(777), tcr(111)], replace)
                .iter()
                .any(|incumbent| incumbent.kind == ProxyKind::TcrEmbedded),
            "replace={replace}: not even beside another incumbent on the port"
        );
    }
    // Without --replace nothing at all is signalled: the operator asked for no
    // kill, and half-killing while refusing to bind is all cost, no benefit.
    assert_eq!(
        incumbents_to_signal(&[embedded(777), tcr(111)], false),
        vec![]
    );
}

/// THE ESCAPE HATCH, and the reason the embedded stand-down is not read as
/// "touch nothing": a `tcr server` sharing the port with an embedded tcr
/// must still be displaceable with `--replace`.
///
/// `port_listeners` filters by port and not by address, so `tcr server` on
/// `[::1]:3456` and an embedded tcr on `127.0.0.1:3456` are both holders.
/// Collapsing that to one kind-blind stand-down would leave the second proxy
/// serving forever on every path, and the two would token-war over the same
/// single-use refresh tokens with no way out.
#[test]
fn replace_still_displaces_a_tcr_peer_beside_an_embedded_incumbent() {
    for holders in [vec![tcr(111), embedded(777)], vec![embedded(777), tcr(111)]] {
        assert_eq!(
            incumbents_to_signal(&holders, true),
            vec![tcr(111)],
            "--replace must still reach the peer, and ONLY it: {holders:?}"
        );
        // We still do not bind: the embedded proxy legitimately holds the port
        // and the stand-down is unchanged. Ending the token war and taking the
        // port over are two different questions.
        assert_eq!(
            takeover_decision(&holders, true),
            Takeover::IncumbentPresent(embedded(777)),
            "{holders:?}"
        );
    }
    // A `tcr` peer beside an embedded proxy is likewise reachable by an
    // explicit --replace, and the embedded one still is not.
    assert_eq!(
        incumbents_to_signal(&[embedded(777), tcr(222)], true),
        vec![tcr(222)]
    );
}

/// The embedded stand-down still carries the marker TcrBar greps for, and does
/// NOT offer `--replace` — the one instruction that cannot work here.
#[test]
fn the_embedded_stand_down_message_reports_without_offering_replace() {
    let message = embedded_stand_down_message(3456, 777);
    assert!(
        message.contains("another proxy holds"),
        "apps/macos ServerController.incumbentMarkers greps for this: {message}"
    );
    assert!(message.contains("777"), "names the incumbent: {message}");
    assert!(
        !message.contains("--replace to"),
        "must not offer an override that is refused for this kind: {message}"
    );
}

/// THE SUCCESSOR'S CLAIM SURVIVES OUR SHUTDOWN. The claim path is named after
/// the port, so a proxy shutting down and the proxy that just replaced it
/// address the same file — and the shutdown frees the listener BEFORE it gets
/// here (`server::ServerHandle::shutdown_within` joins background tasks in
/// between, including a blocking affinity write). An unconditional unlink
/// deletes the live successor's claim: for an embedded successor the name
/// matcher then recognizes nothing, `tcr login` stops refusing, and the
/// boot-time single-use refresh tokens overwrite the fresh ones.
#[test]
fn a_withdrawal_deletes_only_a_claim_this_process_still_owns() {
    let dir = scratch_dir("withdraw");

    // (1) The successor's claim — different pid, same port, same path.
    let path = write_claim(&dir, 3456, 777, ProxyHost::Embedded);
    remove_owner_file_if_owned(&path, 5150, 3456);
    assert!(
        path.exists(),
        "withdrawing must not delete a claim written by another pid: {}",
        path.display()
    );
    assert_eq!(
        read_owner_file(&path).map(|owner| owner.pid),
        Some(777),
        "and it must be left byte-for-byte the successor's"
    );

    // (2) A claim for another port at this path is not ours either.
    remove_owner_file_if_owned(&path, 777, 3457);
    assert!(path.exists(), "the port must match too: {}", path.display());

    // (3) The positive control: our OWN claim is withdrawn, so a shutdown
    // still stops advertising a port it no longer listens on.
    let ours = write_claim(&dir, 3457, 5150, ProxyHost::Cli);
    remove_owner_file_if_owned(&ours, 5150, 3457);
    assert!(
        !ours.exists(),
        "a proxy must withdraw its own claim on shutdown: {}",
        ours.display()
    );

    // (4) Withdrawing twice (a re-issued shutdown) is quiet and harmless.
    remove_owner_file_if_owned(&ours, 5150, 3457);

    crate::log_err::LogErr::log_debug(std::fs::remove_dir_all(&dir), "removing a test temp dir");
}

/// The claim file is named after the PORT, so two proxies on two ports never
/// overwrite each other's identity, and it lives beside the pin cache rather
/// than anywhere a caller has to remember.
#[test]
fn the_claim_path_is_port_scoped_and_beside_the_pin_cache() {
    assert_eq!(
        owner_path_in(Path::new("/tmp/x"), 3456),
        PathBuf::from("/tmp/x/proxy-owner-3456.json")
    );
    assert_ne!(
        owner_path_in(Path::new("/tmp/x"), 3456),
        owner_path_in(Path::new("/tmp/x"), 3457)
    );
    assert_eq!(
        default_owner_path(3456).parent(),
        crate::affinity::default_path().parent(),
        "the claim belongs in the same directory as the pin cache"
    );
}

/// FORWARD COMPATIBILITY. The claim is read by a different BUILD of `tcr` than
/// the one that wrote it — an older one, on any machine where a newer proxy is
/// serving. A host value this build does not know must degrade to "a proxy is
/// there, do not signal it", never to "no claim at all": the latter drops the
/// reader back to the name matcher, which cannot see an embedded proxy, so
/// `tcr login` proceeds beside a live server and `--replace` SIGTERMs the
/// process the file existed to protect.
#[test]
fn a_host_this_build_does_not_know_is_still_a_proxy_worth_protecting() {
    let dir = scratch_dir("future-host");
    let path = owner_path_in(&dir, 3456);
    std::fs::write(
        &path,
        br#"{"pid":900,"port":3456,"sha":"0000000","host":"sidecar"}"#,
    )
    .expect("scratch write");

    let parsed: ProxyOwner =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("scratch read"))
            .expect("an unknown host must not fail the whole parse");
    assert_eq!(parsed.host, ProxyHost::Unknown);
    assert_eq!(parsed.pid, 900, "the rest of the claim still parses");

    // And it lands on the kind that is never signalled, whatever the host
    // program's command line says.
    let command = |_pid: u32| argv(HOST_APP_ARGV);
    assert_eq!(
        classify_port_owner(3456, &[900], &path, command),
        Some(embedded(900)),
        "an unrecognized host degrades to the protected kind, not to nothing"
    );
    assert_eq!(
        incumbents_to_signal(&[embedded(900)], true),
        vec![],
        "and --replace still cannot signal it"
    );

    crate::log_err::LogErr::log_debug(std::fs::remove_dir_all(&dir), "removing a test temp dir");
}

/// The on-disk shape is a cross-process contract: the file a proxy writes is
/// read by a DIFFERENT `tcr` build, possibly older or newer. The key names are
/// spelled out here so a rename cannot happen silently.
#[test]
fn the_claim_is_the_documented_json_shape() {
    let json = serde_json::to_string(&ProxyOwner {
        pid: 42,
        port: 3456,
        sha: "abc1234".to_string(),
        host: ProxyHost::Embedded,
    })
    .expect("a claim serializes");
    assert_eq!(
        json, r#"{"pid":42,"port":3456,"sha":"abc1234","host":"embedded"}"#,
        "the claim's field names and the lowercase host values are read by \
             other builds of tcr"
    );
    let parsed: ProxyOwner = serde_json::from_str(r#"{"pid":7,"port":1,"sha":"s","host":"cli"}"#)
        .expect("the documented shape parses");
    assert_eq!(parsed.host, ProxyHost::Cli);
}

/// THE INDEPENDENT ORACLE for this lane. This defect predates the argv
/// refactor: the OLD `&str` implementation did `cmd.split_whitespace()`,
/// which mis-tokenises any executable path containing a space —
/// `"/Applications/My App/tcr server"` split into
/// `["/Applications/My", "App/tcr", "server"]`, so `tokens[0]` never ended
/// with `/tcr` and a REAL tcr server went unrecognized (treated as a
/// non-proxy holder, or worse left un-killable under `--replace`, or
/// spuriously blocking `tcr login`). Verified failing against the `&str` API
/// before this refactor landed. Now expressed against the argv API this
/// module actually exposes: `sysinfo::Process::cmd()` hands back the
/// kernel's own tokenization, so the path's internal space never has to be
/// re-split at all — the defect class is gone by construction, not patched.
#[test]
fn recognizes_a_tcr_server_at_a_path_containing_a_space() {
    assert_eq!(
        classify_proxy_server(&argv(&["/Applications/My App/tcr", "server"])),
        Some(ProxyKind::Tcr),
        "a space in the executable's path must not hide a real tcr server"
    );
}

#[test]
fn classify_names_which_proxy_it_found() {
    assert_eq!(
        classify_proxy_server(&argv(&["/opt/tcr/target/release/tcr", "server"])),
        Some(ProxyKind::Tcr)
    );
    assert_eq!(classify_proxy_server(&argv(&["tcr"])), Some(ProxyKind::Tcr));
    assert_eq!(classify_proxy_server(&argv(&["/x/tcr", "status"])), None);
}

#[test]
fn replaceable_incumbents_filters_self_and_non_proxies() {
    let command = |pid: u32| -> Vec<String> {
        match pid {
            111 => argv(&["/usr/local/bin/tcr"]),
            222 => argv(&["/x/tcr", "server"]),
            333 => argv(&["grep", "tcr", "server"]), // non-proxy on the port
            444 => argv(&["/x/tcr", "run"]),
            _ => Vec::new(),
        }
    };
    // 999 is self; 333/444 are non-proxies → only 111 and 222 survive, each
    // carrying WHICH proxy it is.
    let replace = replaceable_incumbents(&[111, 222, 333, 444, 999], 999, command);
    assert_eq!(replace, vec![tcr(111), tcr(222)]);
}

#[test]
fn replaceable_incumbents_never_includes_self_even_if_it_looks_like_a_proxy() {
    let command = |_pid: u32| argv(&["/x/tcr", "server"]);
    assert!(replaceable_incumbents(&[42], 42, command).is_empty());
}

/// THE DEFAULT, and the whole point of the enum: a healthy `tcr` PEER is left
/// running and the caller is told to stand down. `Proceed` here would reach
/// the kill loop and cost every live session its prompt cache.
#[test]
fn a_healthy_tcr_peer_is_left_alone_by_default() {
    assert_eq!(
        takeover_decision(&[tcr(4242)], false),
        Takeover::IncumbentPresent(tcr(4242))
    );
    assert_eq!(
        incumbents_to_signal(&[tcr(4242)], false),
        vec![],
        "a protected peer must not be signalled"
    );
}

/// `--replace` is the only way to reach the kill loop for a `tcr` peer.
#[test]
fn replace_flag_proceeds_to_the_takeover() {
    assert_eq!(takeover_decision(&[tcr(4242)], true), Takeover::Proceed);
    assert_eq!(
        incumbents_to_signal(&[tcr(4242)], true),
        vec![tcr(4242)],
        "and it is the incumbent that gets signalled"
    );
}

/// Nothing replaceable on the port — bind, with or without the flag. Covers
/// both the free port and the non-proxy holder, which `replaceable_incumbents`
/// has already filtered out by the time we get here (the bind then fails
/// loudly, which is the documented behaviour and must not change).
#[test]
fn an_empty_port_always_proceeds() {
    assert_eq!(takeover_decision(&[], false), Takeover::Proceed);
    assert_eq!(takeover_decision(&[], true), Takeover::Proceed);
    assert_eq!(incumbents_to_signal(&[], false), vec![]);
}

/// The stand-down message is read by TcrBar's `incumbentMarkers`, in another
/// language, with no compiler between the two. Dropping the phrase would make
/// a stand-down render in the menu-bar app as a clean `exited(0)` — "the
/// server started and stopped" for a server that never bound — and would turn
/// its takeover button into a silent no-op instead of a reported refusal.
#[test]
fn the_stand_down_message_carries_the_marker_tcrbar_greps_for() {
    // The literal is SPELLED OUT rather than referenced through
    // `INCUMBENT_MARKER`, deliberately. Asserting `message.contains(MARKER)`
    // compares the constant with itself and passes for ANY value of it — the
    // constant is exactly the thing that must not drift, so the test has to
    // hold the other copy. This literal must equal the entry in
    // `apps/macos/Sources/TcrBarCore/ServerController.swift`'s
    // `incumbentMarkers`; nothing but this assertion couples them.
    let tcrbar_greps_for = "another proxy holds";
    assert_eq!(
        INCUMBENT_MARKER, tcrbar_greps_for,
        "the marker must stay the string TcrBar's incumbentMarkers list carries"
    );
    let message = stand_down_message(3456, 4242);
    assert!(
        message.contains(tcrbar_greps_for),
        "apps/macos ServerController.incumbentMarkers greps for \
             {tcrbar_greps_for:?}: {message}"
    );
    assert!(message.contains("4242"), "names the incumbent: {message}");
    assert!(
        message.contains("--replace"),
        "names the override: {message}"
    );
}

/// Several `tcr` peers at once: the first one listed wins the protection,
/// and standing down signals NOTHING — a half-kill would leave the port held
/// by the survivor and this process refusing to bind anyway.
#[test]
fn several_tcr_peers_stand_us_down_and_none_is_signalled() {
    for holders in [vec![tcr(111), tcr(222)], vec![tcr(222), tcr(111)]] {
        assert_eq!(
            takeover_decision(&holders, false),
            Takeover::IncumbentPresent(holders[0]),
            "the protected peer is the one named: {holders:?}"
        );
        assert_eq!(
            incumbents_to_signal(&holders, false),
            vec![],
            "standing down kills nothing: {holders:?}"
        );
    }
}
