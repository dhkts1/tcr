//! Unit tests for the library boot path in [`super`] and its peer-mesh side.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn independent_fan_survives_failed_descriptor_send() {
    use tcr_fan::controller::{Command, FanDesired};
    let mut config = boot_line_test_config();
    config.proxy.port = 0;
    config.upstream = "http://127.0.0.1:0".into();
    config.codex_upstream = "http://127.0.0.1:0".into();
    config
        .extra
        .insert("quotaProbeSeconds".into(), serde_json::json!(0));
    config
        .extra
        .insert("warmupSeconds".into(), serde_json::json!(0));
    let mut handle = serve(ServeOptions {
        tls: TlsSetup::Disabled,
        ..ServeOptions::new(config)
    })
    .await
    .unwrap()
    .expect_started();
    let fan = crate::stores::fan::start_inert();
    assert!(
        fan.send(Command::Set(FanDesired::Auto)).is_ok(),
        "positive control: controller must accept before handoff"
    );
    handle.manager().install_fan(Arc::clone(&fan));
    let http = crate::base::http_client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let fan_url = format!("http://{}/_tcr/fan", handle.addr());
    let before_status = http
        .post(&fan_url)
        .json(&serde_json::json!({"mode":"auto"}))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(
        before_status.as_u16(),
        202,
        "positive control: HTTP fan route accepts before handoff"
    );
    let (ours, successor) = std::os::unix::net::UnixStream::pair().unwrap();
    drop(successor);
    let error = handle.handoff(&ours).await.unwrap_err();
    let accepting = !*handle.shutdown.borrow();
    let owning = !handle.manager().mutation_is_released();
    let fan_accepts = fan.send(Command::Set(FanDesired::Auto)).is_ok();
    let after_response = http
        .post(&fan_url)
        .json(&serde_json::json!({"mode":"auto"}))
        .send()
        .await;
    let (after_status, after_body) = match after_response {
        Ok(response) => (response.status().as_u16(), response.text().await.unwrap()),
        Err(error) => (0, error.to_string()),
    };
    let healthy = crate::base::http_client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap()
        .get(format!("http://{}/_tcr/status", handle.addr()))
        .send()
        .await
        .is_ok_and(|r| r.status().is_success());
    handle.shutdown().await;
    assert!(
        accepting && owning && healthy && fan_accepts && after_status == 202,
        "failed pre-send handoff must preserve all service: accepting={accepting}, owning={owning}, http200={healthy}, fan_accepts={fan_accepts}, fan_http_before={before_status}, fan_http_after={after_status}, fan_body={after_body}, error={error}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_handoff_whose_send_fails_keeps_serving_and_owning_the_port() {
    for missing_listener in [false, true] {
        let mut config = boot_line_test_config();
        config.proxy.port = 0;
        config.upstream = "http://127.0.0.1:0".into();
        config.codex_upstream = "http://127.0.0.1:0".into();
        config
            .extra
            .insert("quotaProbeSeconds".into(), serde_json::json!(0));
        config
            .extra
            .insert("warmupSeconds".into(), serde_json::json!(0));
        let mut handle = serve(ServeOptions {
            tls: TlsSetup::Disabled,
            ..ServeOptions::new(config)
        })
        .await
        .unwrap()
        .expect_started();
        let fan = crate::stores::fan::start_inert();
        assert!(
            fan.send(tcr_fan::controller::Command::Set(
                tcr_fan::controller::FanDesired::Auto
            ))
            .is_ok()
        );
        handle.manager().install_fan(Arc::clone(&fan));
        let addr = handle.addr();
        let (ours, successor) = std::os::unix::net::UnixStream::pair().unwrap();
        drop(successor);
        let retained = if missing_listener {
            handle.listener.take()
        } else {
            None
        };
        let error = handle.handoff(&ours).await.unwrap_err();
        let accepting = !*handle.shutdown.borrow();
        let owning = !handle.manager().mutation_is_released();
        let response = crate::base::http_client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap()
            .get(format!("http://{addr}/_tcr/status"))
            .send()
            .await;
        let healthy = response.is_ok_and(|r| r.status().is_success());
        let fan_response = crate::base::http_client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap()
            .post(format!("http://{addr}/_tcr/fan"))
            .json(&serde_json::json!({"mode":"auto"}))
            .send()
            .await;
        let fan_accepted = fan_response.is_ok_and(|response| response.status().as_u16() == 202);
        drop(retained);
        handle.shutdown().await;
        assert!(
            accepting && owning && healthy && fan_accepted,
            "pre-send failure must retain accepting, ownership and HTTP 200: missing={missing_listener}, accepting={accepting}, owning={owning}, healthy={healthy}, fan_accepted={fan_accepted}, error={error}"
        );
    }
}

use super::peer_mesh::path_traffic_tick;
use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_handoff_stops_the_predecessor_fan_before_releasing_the_listener() {
    use std::io::{Read, Write};
    use tcr_fan::controller::{Command, FanDesired};
    let dir = tempfile::tempdir().unwrap();
    let fan = crate::stores::fan::start(dir.path().join("fan.json")).unwrap();
    let manager = inert_manager();
    manager.install_fan(Arc::clone(&fan));
    let (shutdown, _rx) = watch::channel(false);
    let mut handle = handle_full(
        shutdown,
        tokio::spawn(async {}),
        Vec::new(),
        Arc::clone(&manager),
        None,
    );
    handle.listener = Some(std::net::TcpListener::bind("127.0.0.1:0").unwrap());
    let (ours, mut successor) = std::os::unix::net::UnixStream::pair().unwrap();
    let receiver = std::thread::spawn(move || {
        successor
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let listener = tcr_fdpass::recv_listener(&successor).unwrap();
        successor.write_all(b"listener-received\n").unwrap();
        let mut released = [0u8; 18];
        successor.read_exact(&mut released).unwrap();
        assert_eq!(&released, b"listener-released\n");
        drop(listener);
    });
    handle.handoff(&ours).await.unwrap();
    receiver.join().unwrap();
    assert!(manager.mutation_is_released(), "handoff must finish first");
    let still_accepts = fan.send(Command::Set(FanDesired::Auto)).is_ok();
    // Close our own thread even when the assertion below fails.
    tokio::task::spawn_blocking(move || fan.shutdown(Duration::from_secs(1)))
        .await
        .unwrap();
    handle.shutdown().await;
    assert!(
        !still_accepts,
        "predecessor fan controller still accepts mutations after the listener and ownership were released"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_handoff_completes_when_the_fan_controller_has_already_stopped() {
    use std::io::{Read, Write};
    use tcr_fan::controller::{Command, FanDesired};
    let dir = tempfile::tempdir().unwrap();
    let fan = crate::stores::fan::start(dir.path().join("fan.json")).unwrap();
    let manager = inert_manager();
    manager.install_fan(Arc::clone(&fan));
    let (shutdown, _rx) = watch::channel(false);
    let mut handle = handle_full(
        shutdown,
        tokio::spawn(async {}),
        Vec::new(),
        Arc::clone(&manager),
        None,
    );
    handle.listener = Some(std::net::TcpListener::bind("127.0.0.1:0").unwrap());
    let (ours, mut successor) = std::os::unix::net::UnixStream::pair().unwrap();
    let receiver = std::thread::spawn(move || {
        successor
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let listener = tcr_fdpass::recv_listener(&successor).unwrap();
        successor.write_all(b"listener-received\n").unwrap();
        let mut released = [0u8; 18];
        successor.read_exact(&mut released).unwrap();
        assert_eq!(&released, b"listener-released\n");
        drop(listener);
    });
    let stopped_fan = Arc::clone(&fan);
    tokio::task::spawn_blocking(move || stopped_fan.shutdown(Duration::from_secs(1)))
        .await
        .unwrap();
    handle.handoff(&ours).await.unwrap();
    receiver.join().unwrap();
    assert!(manager.mutation_is_released(), "handoff must finish first");
    let still_accepts = fan.send(Command::Set(FanDesired::Auto)).is_ok();
    // Close our own thread even when the assertion below fails.
    tokio::task::spawn_blocking(move || fan.shutdown(Duration::from_secs(1)))
        .await
        .unwrap();
    handle.shutdown().await;
    assert!(
        !still_accepts,
        "predecessor fan controller still accepts mutations after the listener and ownership were released"
    );
}

#[test]
fn worker_threads_default_to_four_or_the_cores_and_honour_the_override() {
    assert_eq!(worker_threads(None, 14), DEFAULT_WORKER_THREADS);
    assert_eq!(worker_threads(None, 2), 2, "never more than the cores");
    assert_eq!(
        worker_threads(None, 0),
        1,
        "a machine reporting no cores still runs one"
    );
    assert_eq!(worker_threads(Some("2"), 14), 2);
    assert_eq!(
        worker_threads(Some(" 16 "), 14),
        16,
        "an explicit count is honoured"
    );
    assert_eq!(worker_threads(Some("0"), 14), DEFAULT_WORKER_THREADS);
    assert_eq!(worker_threads(Some("many"), 14), DEFAULT_WORKER_THREADS);
}

/// A manager with no accounts: nothing to probe, refresh or warm, and
/// `config_path: None` so no file can be written.
fn inert_manager() -> Arc<Manager> {
    manager_with(r#"{"accounts": []}"#)
}

fn manager_with(config: &str) -> Arc<Manager> {
    let config: Config = serde_json::from_str(config).expect("the inline test config parses");
    Manager::with_live_refresher(config, None)
}

fn proxy_doc(path: &std::path::Path) -> serde_json::Value {
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).expect("the config reads"))
            .expect("the config parses");
    doc["proxy"].clone()
}

/// **Boot writes a request key once, and the server's later writes keep it.** A config
/// with none gains one, in the file and in memory; the file's other keys are untouched;
/// a token write (the merge path and its whole-file fallback) keeps it; a second boot
/// keeps the same key; and a server with no file to write holds none.
#[test]
fn boot_generates_a_request_key_that_later_writes_keep() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("tcr.json");
    std::fs::write(
        &path,
        r#"{"proxy": {"port": 3456, "customFlag": true},
                "accounts": [{"name": "alice@example.com", "accessToken": "at-fake",
                              "refreshToken": "rt-fake", "expiresAt": 4102444800000}]}"#,
    )
    .expect("the temp config writes");

    let mut config = crate::base::config::load(&path).expect("the temp config loads");
    assert_eq!(config.proxy.request_key, None);
    ensure_request_key(&mut config, Some(&path));
    let key = config.proxy.request_key.clone().expect("boot holds a key");
    assert_eq!(key.len(), 64);
    assert_eq!(proxy_doc(&path)["requestKey"], key.as_str());
    assert_eq!(proxy_doc(&path)["customFlag"], true, "other keys untouched");
    assert_eq!(
        crate::base::config::read_request_key(&path).expect("reads"),
        Some(key.clone()),
        "`tcr request-key` reads the same key"
    );

    // A later server write: the token merge, from the server's own snapshot.
    config.accounts[0].access_token = "at-rotated".to_string();
    crate::base::config::save_tokens(&path, &config).expect("token write");
    assert_eq!(proxy_doc(&path)["requestKey"], key.as_str());
    // Its whole-file fallback, taken when the file cannot be parsed.
    std::fs::write(&path, "{ not json").expect("break the file");
    crate::base::config::save_tokens(&path, &config).expect("fallback write");
    assert_eq!(proxy_doc(&path)["requestKey"], key.as_str());

    // A second boot keeps it rather than minting another.
    let mut again = crate::base::config::load(&path).expect("reloads");
    ensure_request_key(&mut again, Some(&path));
    assert_eq!(again.proxy.request_key.as_deref(), Some(key.as_str()));
    assert_eq!(proxy_doc(&path)["requestKey"], key.as_str());

    // No file to write: no key, so the server never holds one no launcher can read.
    let mut library: Config = serde_json::from_str("{}").expect("parses");
    ensure_request_key(&mut library, None);
    assert_eq!(library.proxy.request_key, None);
}

/// Another writer that put a key in the file after this process loaded it wins, so
/// the server and `tcr request-key` never hold two different keys.
#[test]
fn boot_adopts_a_key_written_since_the_config_was_loaded() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("tcr.json");
    std::fs::write(&path, r#"{"accounts": []}"#).expect("writes");
    let mut config = crate::base::config::load(&path).expect("loads");
    std::fs::write(
            &path,
            r#"{"proxy": {"requestKey": "3333333333333333333333333333333333333333333333333333333333333333"}, "accounts": []}"#,
        )
        .expect("another writer");
    ensure_request_key(&mut config, Some(&path));
    assert_eq!(
        config.proxy.request_key.as_deref(),
        Some("3333333333333333333333333333333333333333333333333333333333333333")
    );
}

/// A path under an existing REGULAR FILE, so any write to it fails. Never
/// near [`affinity::default_path`] — a test may not touch the live cache.
fn unwritable_path(tag: &str) -> PathBuf {
    let blocker = std::env::temp_dir().join(format!(
        "tcr-server-unit-{}-{tag}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or_default(|d| d.as_nanos())
    ));
    std::fs::write(&blocker, b"not a directory").expect("the scratch blocker file is writable");
    blocker.join("affinity.json")
}

/// A handle owning `server` plus `background`, wired to `shutdown`, that can
/// touch nothing outside this process when dropped.
fn handle_owning(
    shutdown: watch::Sender<bool>,
    server: JoinHandle<()>,
    background: Vec<JoinHandle<()>>,
) -> ServerHandle {
    handle_full(shutdown, server, background, inert_manager(), None)
}

fn handle_full(
    shutdown: watch::Sender<bool>,
    server: JoinHandle<()>,
    background: Vec<JoinHandle<()>>,
    manager: Arc<Manager>,
    affinity_path: Option<PathBuf>,
) -> ServerHandle {
    ServerHandle {
        listener: None,
        connections: None,
        addr: "127.0.0.1:0"
            .parse()
            .expect("a literal loopback addr parses"),
        shutdown,
        manager,
        affinity_path,
        // Same reasoning as `affinity_path` above: a hand-built handle in a
        // unit test writes nothing outside this process.
        wire_sessions_path: None,
        // No claim: a hand-built handle in a unit test may not delete a file
        // on shutdown, least of all one named after the live proxy's port.
        owner_path: None,
        tasks_joined: 0,
        tasks_aborted: 0,
        server: Some(server),
        server_finished: false,
        background,
        // No peer socket: a hand-built handle in a unit test opens no port
        // at all, least of all a second one on the LAN.
        peer_addr: None,
        successor: None,
    }
}

async fn all_finished_within(
    aborts: &[tokio::task::AbortHandle],
    budget: Duration,
) -> Result<(), usize> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let live = aborts.iter().filter(|a| !a.is_finished()).count();
        if live == 0 {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(live);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// `Drop` must ABORT. Every task here ignores the shutdown channel entirely,
/// so nothing but `JoinHandle::abort` can end it — and a retained `Sender`
/// clone keeps the channel open, so closing it is not available either.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_the_handle_aborts_tasks_that_ignore_the_shutdown_signal() {
    let (shutdown, _rx) = watch::channel(false);
    let keepalive = shutdown.clone();

    let deaf = || tokio::spawn(std::future::pending::<()>());
    let server = deaf();
    let background: Vec<JoinHandle<()>> = (0..3).map(|_| deaf()).collect();
    let aborts: Vec<tokio::task::AbortHandle> = std::iter::once(server.abort_handle())
        .chain(background.iter().map(JoinHandle::abort_handle))
        .collect();

    let handle = handle_owning(shutdown, server, background);
    assert_eq!(
        aborts.iter().filter(|a| a.is_finished()).count(),
        0,
        "the tasks must still be running before the handle is dropped"
    );

    drop(handle);

    match all_finished_within(&aborts, Duration::from_secs(5)).await {
        Ok(()) => {}
        Err(live) => panic!(
            "Drop leaked {live} of {} tasks: a task that ignores the shutdown \
                 channel can only be stopped by `abort()`",
            aborts.len()
        ),
    }
    drop(keepalive);
}

/// `shutdown` must not be able to HANG on a task that will not stop.
///
/// The affinity flusher writes with blocking `std::fs` inside async code, so
/// on a full or wedged filesystem it reaches no cancellation point; an
/// unbounded join there meant `tcr` quitting into a hang with the terminal
/// already restored, nothing serving, and `persist_now` never reached. A
/// `pending()` task is that condition with the filesystem left out of it.
///
/// The outer `timeout` is the assertion: it is 30x the grace, so it can only
/// fire if the join is unbounded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_aborts_a_task_that_will_not_stop_instead_of_hanging() {
    let (shutdown, _rx) = watch::channel(false);
    let keepalive = shutdown.clone();
    let wedged = tokio::spawn(std::future::pending::<()>());
    let wedged_abort = wedged.abort_handle();
    let mut handle = handle_owning(shutdown, tokio::spawn(std::future::ready(())), vec![wedged]);

    let grace = Duration::from_millis(100);
    let report = tokio::time::timeout(grace * 30, handle.shutdown_within(grace))
        .await
        .expect("shutdown hung on a task that never stops");

    assert_eq!(report.tasks_joined, 1, "the accept loop stopped on its own");
    assert_eq!(
        report.tasks_aborted, 1,
        "the wedged task must be aborted, not waited for"
    );
    assert!(
        wedged_abort.is_finished() || {
            tokio::time::sleep(grace).await;
            wedged_abort.is_finished()
        },
        "the wedged task was abandoned rather than aborted"
    );
    drop(keepalive);
}

/// `shutdown` must be cancel-safe and re-issuable.
///
/// It used to consume `self`, so a caller bounding it with a deadline — the
/// pattern the integration test itself models — lost the handle along with
/// the future, skipping `persist_now` and the final pin write with nothing
/// left to retry. Here the first attempt is cancelled mid-join and the second
/// still accounts for every task.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_shutdown_can_be_re_issued_and_still_accounts_for_every_task() {
    let (shutdown, _rx) = watch::channel(false);
    let keepalive = shutdown.clone();
    // Ignores the signal for 300ms, then stops: long enough for the first
    // attempt to be cancelled while joining it. The accept loop is already
    // finished, so the cancellation lands inside the BACKGROUND join — the
    // loop whose bookkeeping is what cancel-safety is about. (Cancelling on
    // the server join instead leaves the background vec untouched, which a
    // consuming implementation would also survive.)
    let slow = || {
        tokio::spawn(async {
            tokio::time::sleep(Duration::from_millis(300)).await;
        })
    };
    let mut handle = handle_owning(
        shutdown,
        tokio::spawn(std::future::ready(())),
        vec![slow(), slow()],
    );

    assert!(
        tokio::time::timeout(
            Duration::from_millis(20),
            handle.shutdown_within(Duration::from_secs(30))
        )
        .await
        .is_err(),
        "the first attempt was supposed to be cancelled mid-join"
    );

    // The handle survived the cancellation, and every task it still owns is
    // joinable — nothing was dropped on the floor by the abandoned future.
    let report = tokio::time::timeout(Duration::from_secs(5), handle.shutdown())
        .await
        .expect("the re-issued shutdown must finish");
    assert_eq!(
        report.tasks_joined + report.tasks_aborted,
        3,
        "every task must be accounted for across the cancelled and re-issued \
             attempts, saw {report:?}"
    );
    assert_eq!(
        report.tasks_aborted, 0,
        "the tasks stop well inside the grace, so none should need aborting: {report:?}"
    );
    drop(keepalive);
}

/// FINDING 2 (round 3). `shutdown_within` promises a caller that bounds it
/// with its own deadline and drops the future keeps a usable handle. The
/// usage-ledger drain is synchronous through and through — a `try_send`
/// retry loop, a `recv_timeout`, a writer-finish poll — and it can occupy
/// the whole remaining grace. Called inline it is a multi-second stretch of
/// an `async fn` with no await point in it, and cancellation only lands at
/// an await: the caller's timeout, which exists to turn a wedged shutdown
/// into a bounded quit, is silently inert, and a Tokio worker is held off
/// its other tasks for the duration.
///
/// The ledger writer here is wedged for the whole test, so the drain will
/// take its full budget; the assertion is that the CALLER's much shorter
/// timeout is the one that decides.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_caller_can_interrupt_a_wedged_usage_ledger_drain() {
    let dir = std::env::temp_dir().join(format!(
        "tcr-server-ledger-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    crate::base::log_err::LogErr::log_debug(
        std::fs::remove_dir_all(&dir),
        "removing a test temp dir",
    );
    let (shutdown, _rx) = watch::channel(false);
    let manager = inert_manager();
    manager.attach_usage_ledger(dir.clone(), 90);
    let wedge = manager.wedge_usage_writer_for_test();

    let mut handle = handle_full(
        shutdown,
        tokio::spawn(std::future::ready(())),
        Vec::new(),
        Arc::clone(&manager),
        None,
    );

    let started = std::time::Instant::now();
    let interrupted = tokio::time::timeout(
        Duration::from_millis(150),
        handle.shutdown_within(Duration::from_secs(5)),
    )
    .await;
    let waited = started.elapsed();

    assert!(
        interrupted.is_err(),
        "the caller's 150ms timeout must be what ends this wait, not the ledger's \
             5s budget"
    );
    assert!(
        waited < Duration::from_secs(2),
        "a blocking drain inside the async fn cannot be cancelled at all: waited \
             {waited:?}"
    );

    // The drain is still running on the blocking pool, which is the point:
    // freeing the writer lets it finish on its own.
    drop(wedge);
    crate::base::log_err::LogErr::log_debug(
        std::fs::remove_dir_all(&dir),
        "removing a test temp dir",
    );
}

/// A FAILED final pin write must be distinguishable from affinity being off.
///
/// Both were `affinity_pins_written: None`, and the difference lived only in
/// a `tracing::warn!` that a library caller — which installs no subscriber —
/// never sees. So total pin loss reported as a clean shutdown.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_final_pin_write_is_reported_not_swallowed() {
    let path = unwritable_path("flush-fails");
    let (shutdown, _rx) = watch::channel(false);
    let mut handle = handle_full(
        shutdown,
        tokio::spawn(std::future::ready(())),
        Vec::new(),
        manager_with(r#"{"sessionAffinity": true, "accounts": []}"#),
        Some(path.clone()),
    );

    let report = handle.shutdown().await;
    assert!(
        report.affinity.failed(),
        "a pin write to {} cannot have succeeded; report said {:?}",
        path.display(),
        report.affinity
    );
    assert_eq!(
        report.affinity.written(),
        None,
        "a failed write wrote no pins"
    );
    assert_ne!(
        report.affinity,
        CacheFlush::Disabled,
        "a failed write must not read as 'affinity is off'"
    );
}

/// With no pin cache path the shutdown flush must be a no-op, not a write to
/// some default — this is the guard on `affinity_path: None` meaning
/// "in memory only".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_pin_cache_path_means_no_final_write() {
    let (shutdown, _rx) = watch::channel(false);
    let mut handle = handle_full(
        shutdown,
        tokio::spawn(std::future::ready(())),
        Vec::new(),
        manager_with(r#"{"sessionAffinity": true, "accounts": []}"#),
        None,
    );
    assert_eq!(handle.shutdown().await.affinity, CacheFlush::Disabled);
}

/// A token minted inside the bound-token flusher's minute must reach the disk on a CLEAN
/// shutdown. The flusher used to stop on the signal without writing, so every restart lost
/// the newest mints, which are exactly the live conversations' tokens; after the next boot
/// their requests found bound history and no minted token to route by.
///
/// The timer's first tick fires at once, so the test waits past it before recording; the
/// next tick is a minute away, so only the final write on the shutdown signal can have
/// written the token.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_clean_shutdown_writes_the_bound_token_map() {
    let dir = std::env::temp_dir().join(format!(
        "tcr-server-bound-tokens-{}-{}",
        std::process::id(),
        crate::now_ms()
    ));
    std::fs::create_dir_all(&dir).expect("creating the test's temp dir");
    let affinity_path = dir.join("session-affinity.json");
    let config = boot_line_test_config();
    let accounts = config.accounts.clone();

    let mut handle = serve(ServeOptions {
        tls: TlsSetup::Disabled,
        affinity_path: Some(affinity_path.clone()),
        ..ServeOptions::new(config)
    })
    .await
    .expect("serving on port 0")
    .expect_started();

    tokio::time::sleep(Duration::from_millis(300)).await;
    let token = "msg_fake_1";
    handle.manager().record_bound_tokens(
        &[(
            crate::stores::bound_tokens::BoundTokenKind::MessageId,
            token.to_string(),
        )],
        0,
        crate::now_ms(),
    );
    handle.shutdown().await;

    let report = crate::stores::bound_tokens::load(
        &crate::stores::bound_tokens::path_beside(&affinity_path),
        &accounts,
        crate::now_ms(),
        crate::stores::bound_tokens::BOUND_TOKEN_TTL_MS,
    );
    let restored = report
        .tokens
        .get(&crate::stores::bound_tokens::hash_token(token))
        .map(|(idx, kind, _)| (*idx, *kind));
    std::fs::remove_dir_all(&dir).expect("removing the test's temp dir");
    assert_eq!(
        restored,
        Some((0, crate::stores::bound_tokens::BoundTokenKind::MessageId)),
        "a token minted before a clean shutdown must be on disk, bound to account 0; \
             load report: degraded={:?} restored={}",
        report.degraded,
        report.tokens.len()
    );
}

/// The bound-token final write belongs to the handle, not to the flusher task. A handle with
/// NO background task (a flusher aborted at the grace deadline looks the same from here) used
/// to lose it; and a clean map is reported as untouched, not rewritten.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_handle_writes_the_bound_token_map_without_a_flusher_task() {
    let dir = std::env::temp_dir().join(format!(
        "tcr-server-bound-final-{}-{}",
        std::process::id(),
        crate::now_ms()
    ));
    std::fs::create_dir_all(&dir).expect("creating the test's temp dir");
    let affinity_path = dir.join("session-affinity.json");
    let bound_path = crate::stores::bound_tokens::path_beside(&affinity_path);
    let (shutdown, _rx) = watch::channel(false);
    let mut handle = handle_full(
        shutdown,
        tokio::spawn(std::future::ready(())),
        Vec::new(),
        manager_with(r#"{"sessionAffinity": false, "accounts": []}"#),
        Some(affinity_path),
    );

    let clean = handle.shutdown().await;
    assert_eq!(clean.bound_tokens, CacheFlush::Unchanged);
    assert!(!bound_path.exists(), "a clean map writes no file");

    handle.manager().record_bound_tokens(
        &[(
            crate::stores::bound_tokens::BoundTokenKind::MessageId,
            "msg_fake_2".to_string(),
        )],
        0,
        crate::now_ms(),
    );
    let report = handle.shutdown().await;
    let on_disk = bound_path.exists();
    std::fs::remove_dir_all(&dir).expect("removing the test's temp dir");
    // No account is configured here, so the entry count is not the point: a write happened.
    assert!(
        report.bound_tokens.written().is_some(),
        "a dirty map must be written, got {:?}",
        report.bound_tokens
    );
    assert!(on_disk, "the final write put the file on disk");
}

/// `Drop` must also SIGNAL — the accept loop and every background loop stop
/// on `stop.changed()`, and the sender-close that currently masks this is
/// removed here by holding a `Sender` clone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_the_handle_signals_tasks_that_watch_the_shutdown_channel() {
    let (shutdown, _rx) = watch::channel(false);
    let keepalive = shutdown.clone();

    // Ends only on a `true`: a bare close would leave `changed()` returning
    // `Err` and this loop spinning back to `pending`, never finishing.
    let watcher = |mut stop: watch::Receiver<bool>| {
        tokio::spawn(async move {
            loop {
                if stop.changed().await.is_err() {
                    std::future::pending::<()>().await;
                }
                if *stop.borrow_and_update() {
                    return;
                }
            }
        })
    };
    let server = watcher(shutdown.subscribe());
    let background: Vec<JoinHandle<()>> = (0..2).map(|_| watcher(shutdown.subscribe())).collect();
    let aborts: Vec<tokio::task::AbortHandle> = std::iter::once(server.abort_handle())
        .chain(background.iter().map(JoinHandle::abort_handle))
        .collect();

    drop(handle_owning(shutdown, server, background));

    assert!(
        *keepalive.borrow(),
        "Drop must publish `true` on the shutdown channel, not merely close it"
    );
    if let Err(live) = all_finished_within(&aborts, Duration::from_secs(5)).await {
        panic!("{live} task(s) never saw the shutdown signal");
    }
    drop(keepalive);
}

// --- background task supervision (part 1) --------------------------------

use crate::test_support::log_capture::SharedBuf;

/// THE catch: `supervise` must swallow a panic from the wrapped future so
/// the spawned task's own `JoinHandle` completes **normally** (this is
/// what lets `ServerHandle::shutdown_within`/`Drop` stay unchanged — see
/// `supervise`'s doc-comment) while still logging which task died and why,
/// via `tracing::error!`. `#[tokio::test]` defaults to current-thread, so
/// the thread-local subscriber set below stays in effect for the spawned
/// task too (same reasoning as `proxy.rs`'s usage-log test).
#[tokio::test]
async fn a_panicking_background_task_is_logged_and_not_respawned() {
    let sink = SharedBuf::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(sink.clone())
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let handle = tokio::spawn(supervise("test-quota-prober", async {
        panic!("boom: deliberate test panic");
    }));

    let result = handle.await;
    assert!(
        result.is_ok(),
        "supervise() must catch the panic so the spawned task's own \
             JoinHandle completes normally instead of returning a JoinError; \
             saw {result:?}"
    );

    let log = sink.contents();
    assert!(
        log.contains("test-quota-prober"),
        "the log line must name which task died: {log:?}"
    );
    assert!(
        log.contains("background task panicked"),
        "the log line must say the task panicked: {log:?}"
    );
    assert!(
        log.contains("boom: deliberate test panic"),
        "the log line must carry the panic message: {log:?}"
    );
}

/// A task that finishes WITHOUT panicking must log nothing — `supervise`
/// only reacts to a panic, never to an ordinary return (which is exactly
/// what every one of these loops does today on a clean shutdown: the
/// `tokio::select!` around `stop.changed()` returns `()`, not a panic).
///
/// Same process-wide callsite-caching problem as
/// `boot_line_carries_every_new_knob_with_its_configured_value` above: if
/// some other thread reached `supervise`'s `tracing::error!` first with no
/// subscriber installed, the interest for that callsite is cached as
/// `never` and this test's sink would stay empty regardless of what
/// `supervise` actually did: an empty sink would pass whether or not a
/// clean return gets logged as a panic. `rebuild_interest_cache()` fixes
/// that, but only proves the callsite is *reachable*; it says nothing
/// about whether THIS assertion could ever fail. So before trusting the
/// empty-sink assertion below, drive the actual panic path first and
/// require the sink to capture it: a positive control. Only then is an
/// empty sink from the clean-return path evidence of anything.
#[tokio::test]
async fn a_clean_return_is_not_logged_as_a_panic() {
    let sink = SharedBuf::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(sink.clone())
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    tracing::callsite::rebuild_interest_cache();

    // Positive control: the sink must actually capture a panic logged by
    // `supervise` under this subscriber, or the assertion below proves
    // nothing.
    let control_handle = tokio::spawn(supervise("test-positive-control", async {
        panic!("positive control: this must reach the sink");
    }));
    control_handle
        .await
        .expect("supervise() catches the panic, so this JoinHandle completes normally");
    assert!(
        sink.contents().contains("background task panicked"),
        "positive control failed: a real panic was not captured, so an \
             empty sink below would prove nothing: {:?}",
        sink.contents()
    );
    sink.clear();

    let handle = tokio::spawn(supervise("test-clean-task", async {}));
    handle
        .await
        .expect("a non-panicking task never returns a JoinError");

    assert!(
        sink.contents().is_empty(),
        "a clean return must not be logged as a panic: {:?}",
        sink.contents()
    );
}

// --- boot-line knob completeness (part 2) ---------------------------------

/// A config with every new boot-line knob set to a **non-default** value,
/// plus one fabricated (non-real) account so `lockAccount`/`controlAccount`
/// each resolve to a real index instead of logging the "did not match any
/// account" error path. Port 0 (ephemeral) so this test can never contend
/// the live proxy's port, and `tls: Disabled` so it touches no TLS material
/// on disk. `access_token` is a fixed non-secret placeholder — this repo is
/// public (see `CLAUDE.md`).
fn boot_line_test_config() -> Config {
    serde_json::from_str(
        r#"{
                "proxy": { "port": 0 },
                "sessionAffinity": false,
                "revalidationServe": false,
                "loadBalanceMigration": true,
                "quotaProbeSeconds": 1234,
                "warmupSeconds": 777,
                "pacing": { "minSpacingMs": 500 },
                "accountThrottle": {},
                "fleetThrottle": {},
                "lockAccount": "test-fixture-account",
                "controlAccount": "test-fixture-account",
                "accounts": [
                    {
                        "name": "test-fixture-account",
                        "accessToken": "not-a-real-token"
                    }
                ]
            }"#,
    )
    .expect("the inline test config parses")
}

/// A handed-over socket is adopted, not re-bound.
///
/// The assertion is deliberately the CONNECT, not just the address. `serve`
/// takes ownership of the descriptor, so if it dropped it and bound its own
/// the original socket would close and connecting to `addr` would be
/// refused. Comparing addresses alone would also catch that in practice,
/// but only because an ephemeral re-bind almost never lands on the same
/// port -- "almost never" is not what this should rest on.
#[tokio::test]
async fn an_inherited_listener_is_adopted_rather_than_rebound() {
    let predecessor =
        std::net::TcpListener::bind("127.0.0.1:0").expect("binding the predecessor's socket");
    let addr = predecessor.local_addr().expect("reading the bound address");
    let (channel, _predecessor_end) =
        std::os::unix::net::UnixStream::pair().expect("a handoff channel");

    let mut handle = serve(ServeOptions {
        tls: TlsSetup::Disabled,
        handover: Some(crate::stores::restart::Handover {
            listener: predecessor,
            predecessor: channel,
        }),
        ..ServeOptions::new(boot_line_test_config())
    })
    .await
    .expect("serving on an inherited listener")
    .expect_started();

    assert_eq!(
        handle.addr(),
        addr,
        "the proxy must serve on the inherited port, not one it bound itself"
    );

    std::net::TcpStream::connect(addr)
        .expect("the inherited socket must still accept connections after adoption");

    handle.shutdown().await;
}

/// Today's requests on account 0, as `tcr status` would report them.
fn requests_today(manager: &Manager) -> u64 {
    manager
        .snapshot(time::OffsetDateTime::now_utc())
        .accounts
        .first()
        .and_then(|account| account.usage.as_ref())
        .map_or(0, |usage| usage.today.requests)
}

fn one_request(now: i64) -> crate::records::usage::UsageRecord {
    crate::records::usage::UsageRecord::from_quota_input(
        now,
        Some("claude-opus-5".to_string()),
        None,
        10,
        0,
        0,
        0,
        1,
    )
}

/// The successor half of a handoff's usage accounting. It replays the day
/// file at boot while the predecessor is still draining, so the
/// predecessor's later lines reach the file after that replay. Once the
/// handoff channel closes the successor must count them, and count its own
/// requests from the same window exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_successor_counts_its_predecessors_last_lines_once_the_channel_closes() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let usage_dir = dir.path().join("usage");
    let mut config = boot_line_test_config();
    config.upstream = "http://127.0.0.1:0".into();
    config.codex_upstream = "http://127.0.0.1:0".into();
    config
        .extra
        .insert("quotaProbeSeconds".into(), serde_json::json!(0));
    config
        .extra
        .insert("warmupSeconds".into(), serde_json::json!(0));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a listener");
    let (channel, predecessor_end) =
        std::os::unix::net::UnixStream::pair().expect("a handoff channel");
    let mut handle = serve(ServeOptions {
        tls: TlsSetup::Disabled,
        usage_dir: Some(usage_dir.clone()),
        handover: Some(crate::stores::restart::Handover {
            listener,
            predecessor: channel,
        }),
        ..ServeOptions::new(config)
    })
    .await
    .expect("serving on a handed-over listener")
    .expect_started();
    let manager = Arc::clone(handle.manager());
    let now = crate::now_ms();

    // The predecessor, still draining, writes four lines after the boot replay.
    let names = [crate::records::usage::LedgerAccount {
        name: "test-fixture-account".to_string(),
        ..crate::records::usage::LedgerAccount::default()
    }];
    let predecessor =
        crate::records::usage::UsageTracker::new(1, crate::base::pricing::PricingTable::default());
    predecessor.attach_ledger(usage_dir.clone(), 90, &names, now);
    for _ in 0..4 {
        predecessor.record(0, &one_request(now), &names[0].name, None, None);
    }
    assert_eq!(
        predecessor.shutdown_ledger(Duration::from_secs(5)),
        crate::records::usage::LedgerShutdown::Flushed
    );
    // The successor serves two of its own in the same window.
    for _ in 0..2 {
        manager.record_usage(0, one_request(now));
    }
    assert_eq!(
        requests_today(&manager),
        2,
        "positive control: before the channel closes the predecessor's lines are not counted"
    );

    // The predecessor's ledger is on disk and it exits.
    drop(predecessor_end);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while requests_today(&manager) != 6 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let counted = requests_today(&manager);
    handle.shutdown().await;
    assert_eq!(
        counted, 6,
        "the predecessor's four lines and the successor's two, each counted once"
    );
}

/// The predecessor half: the handoff channel stays open until the
/// predecessor's usage ledger is on disk, because its close is what tells
/// the successor the day file is complete. A request that finishes during
/// the drain queues a line that only the final shutdown writes, so a
/// channel that closed when the drain ended would let the successor count
/// the file without it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_handoff_channel_closes_only_after_the_predecessors_ledger_is_on_disk() {
    use std::io::{Read, Write};
    let dir = tempfile::tempdir().expect("a temp dir");
    let usage_dir = dir.path().join("usage");
    let manager = manager_with(
        r#"{"accounts": [{"name": "alice@example.com", "accessToken": "at-fake"}],
                "quotaProbeSeconds": 0, "warmupSeconds": 0}"#,
    );
    manager.attach_usage_ledger(usage_dir.clone(), 90);
    let (shutdown, _rx) = watch::channel(false);
    let mut handle = handle_full(
        shutdown,
        tokio::spawn(async {}),
        Vec::new(),
        Arc::clone(&manager),
        None,
    );
    handle.listener = Some(std::net::TcpListener::bind("127.0.0.1:0").expect("a listener"));
    let now = crate::now_ms();
    let (ours, mut successor) = std::os::unix::net::UnixStream::pair().expect("a channel");
    let (closed_tx, closed_rx) = tokio::sync::oneshot::channel();
    let reader_dir = usage_dir.clone();
    let receiver = std::thread::spawn(move || {
        successor
            .set_read_timeout(Some(Duration::from_secs(60)))
            .expect("a read timeout");
        let listener = tcr_fdpass::recv_listener(&successor).expect("the listener arrives");
        successor
            .write_all(b"listener-received\n")
            .expect("the receipt is sent");
        let mut released = [0u8; 18];
        successor
            .read_exact(&mut released)
            .expect("the release is confirmed");
        drop(listener);
        // What the successor reads when the channel closes.
        let mut rest = Vec::new();
        let closed = successor.read_to_end(&mut rest);
        let on_disk = crate::records::usage::parse_ledger_day(
            &reader_dir,
            crate::records::usage::ledger_date(now),
        )
        .0
        .len();
        closed_tx
            .send((closed.map(|_| ()).map_err(|e| e.to_string()), on_disk))
            .expect("the test is waiting for this");
    });
    handle.handoff(&ours).await.expect("the handoff completes");

    // A request finishes during the drain, and the writer is held so its
    // line is provably still queued.
    let wedge = manager.wedge_usage_writer_for_test();
    manager.record_usage(0, one_request(now));
    // The binary drops its own copy of the channel when the drain ends.
    drop(ours);
    let mut closed_rx = closed_rx;
    let early = tokio::time::timeout(Duration::from_millis(500), &mut closed_rx).await;
    drop(wedge);
    handle.shutdown().await;
    let (closed, on_disk) = match early {
        Ok(result) => result,
        Err(_) => tokio::time::timeout(Duration::from_secs(10), closed_rx)
            .await
            .expect("the channel closes once shutdown has written the ledger"),
    }
    .expect("the receiver reported");
    receiver.join().expect("the receiver thread finished");
    assert_eq!(
        (closed, on_disk),
        (Ok(()), 1),
        "the channel closed before the predecessor's last line was on disk"
    );
}

/// THE bite: every knob this unit added to the "server started" boot line
/// must actually be ON that line, with the value this config set — not
/// merely present in config/`Manager`. A caller diagnosing a cold-cache
/// incident reads this ONE line (see the module doc-comment on why it is
/// one line, not several); a knob resolved correctly but never logged is
/// invisible to exactly that read.
///
/// `flavor = "current_thread"` is pinned explicitly, not left to the
/// macro's default: `_guard` below is a thread-local `DefaultGuard`
/// (`tracing::subscriber::set_default`), so it only covers whichever OS
/// thread runs the test body. `serve()`'s boot line is emitted inline,
/// on the calling task, and `background.push(tokio::spawn(...))`'s
/// loops spawned earlier in `serve()` are themselves polled on that same
/// task's thread under `current_thread` (there is only one), so nothing
/// here can land on a thread the guard never reached. Pinning the flavor
/// makes that invariant survive a future default change instead of
/// resting on it silently (same reasoning as
/// `a_panicking_background_task_is_logged_and_not_respawned` above).
///
/// # Why this boots more than once, and waits on no clock
///
/// The `server started` emit is one process-wide `tracing` callsite, and
/// `tracing` caches that callsite's `Interest` globally the FIRST time any
/// thread reaches it. When only one dispatcher is registered,
/// `callsite::register` computes that interest from the REGISTERING
/// thread's own default subscriber (`Rebuilder::JustOne` in
/// `tracing-core`'s `callsite.rs`). Two other tests in this binary boot a
/// server with no subscriber installed. Whichever of them reaches the
/// callsite first caches `Interest::never` for it, and from then on THIS
/// test's `serve()` emits nothing at all, however long anything waits: the
/// line is lost, not late.
///
/// Measured on 2026-09-19 at this exact callsite: with a bare thread
/// booting first, a `serve()` on the capturing thread wrote no boot line to
/// the sink, and a `rebuild_interest_cache()` on the capturing thread made
/// the next boot write it. That is what the two calls below are: the
/// rebuild runs while this thread's capturing subscriber is the default, so
/// it recomputes the interest for every callsite already registered,
/// including this one. A callsite that is registered for the first time
/// DURING the first boot here is out of that rebuild's reach, so the
/// attempt after it repairs that case: registration has happened by then,
/// and the second attempt's rebuild covers it.
///
/// This replaced a 5 s deadline that reddened three full `--lib` runs under
/// load and passed alone every time. The deadline could not have worked:
/// the line was never emitted in those runs, so no deadline is long enough.
/// Nothing here reads a clock.
#[tokio::test(flavor = "current_thread")]
async fn boot_line_carries_every_new_knob_with_its_configured_value() {
    /// Two are enough for the mechanism above, three is margin.
    const BOOTS: usize = 3;

    let mut captured = String::new();
    for _ in 0..BOOTS {
        let sink = SharedBuf::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(sink.clone())
            .with_ansi(false)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        tracing::callsite::rebuild_interest_cache();

        let mut handle = serve(ServeOptions {
            tls: TlsSetup::Disabled,
            ..ServeOptions::new(boot_line_test_config())
        })
        .await
        .expect("bind must succeed on an ephemeral port")
        .expect_started();

        // Read once. The emit is inline in `serve()` (see the `tracing::info!`
        // beside the bind), so by the time `serve()` has returned the line is
        // either in this sink or it was never enabled for this boot.
        let line = sink
            .contents()
            .lines()
            .find(|line| line.contains("server started"))
            .map(str::to_string);
        handle.shutdown().await;
        captured = sink.contents();

        let Some(boot_line) = line else {
            continue;
        };
        assert_boot_line_knobs(boot_line.as_str());
        return;
    }
    panic!("no \"server started\" line in {BOOTS} boots; captured log: {captured:?}");
}

/// Every knob this unit added to the boot line, with the value
/// [`boot_line_test_config`] set.
fn assert_boot_line_knobs(boot_line: &str) {
    for expected in [
        "session_affinity=false",
        "revalidation_serve=false",
        "load_balance_migration=true",
        "quota_probe_seconds=1234",
        "warmup_seconds=777",
        "pacing_active=true",
        // An empty `{}` object is the documented per-bucket escape hatch
        // (see `ThrottleConfig`'s doc-comment) — it overrides that bucket's
        // default-ON setting to fully inert. `throttle_active` is the OR of
        // the two buckets, so BOTH must be `{}` to make it false, and
        // `false` here IS the non-default assertion: the default build's
        // boot line reads `throttle_active=true`.
        //
        // Note this fixture goes through `serde_json::from_str`, not
        // `config::load`, so it does NOT get the legacy-`throttle`-key
        // rejection. That is exactly how this test caught the rename: a
        // stale `"throttle": {}` here landed silently in the `extra`
        // catch-all and both buckets defaulted back ON.
        "throttle_active=false",
        "lock_account=\"test-fixture-account\"",
        "control_account=\"test-fixture-account\"",
    ] {
        assert!(
            boot_line.contains(expected),
            "boot line missing {expected:?}; full line: {boot_line:?}"
        );
    }
}

/// **The peer-lease fallback is wired at boot**, a server booted beside a
/// peers file with sharing on reaches the provider on a dry fleet.
///
/// `install_peer_lease_provider` had no production caller at all: the whole
/// feature was a library function and a test, so a `tcr` that had paired a
/// Mac and granted it `disclose` still answered the honest 429 without ever
/// asking that Mac for a lease.
///
/// The instrument is a socket, not a log line and not the `OnceLock`. The
/// provider's first act on a dry-fleet ask is to dial the lender's address
/// (`lease::request_lease` -> `serve::dial_peer`), so a listener at that
/// address that accepts and hangs up is proof the arm reached the provider
/// and the provider read the peers file. It hangs up rather than answering,
/// which fails the borrower's handshake and returns it to the last rung of
/// the ladder, the 429 this handler always had.
///
/// Watch it fail by deleting the `install_peer_lease_provider` block from
/// `serve`: no connection arrives, `configured_provider()` is `None`, and
/// both assertions below go red. Measured that way before this test was
/// kept.
///
/// It is the only test in this binary that may install a provider, the
/// install is process-wide by design (`crate::peer::serve::fallback::PROVIDER`).
#[tokio::test(flavor = "multi_thread")]
async fn booting_beside_a_sharing_peers_file_wires_the_peer_lease_fallback() {
    let home = tempfile::tempdir().expect("a temp home");
    let config_path = home.path().join(crate::base::paths::CONFIG_FILE);
    std::fs::write(&config_path, br#"{"accounts": []}"#).expect("the temp config writes");

    // The lender: a socket that accepts one connection, records it, and
    // closes. Obviously fake node id, and an address on a kernel port.
    let lender = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the fake lender");
    let lender_addr = lender.local_addr().expect("the fake lender's addr");
    let dialled = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = Arc::clone(&dialled);
    tokio::spawn(async move {
        while let Ok((stream, _)) = lender.accept().await {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            drop(stream);
        }
    });

    // The peers file BESIDE the config, which is how `serve` resolves it.
    // See `peers_file_beside_config`. `disclose` on plus an address is what
    // "sharing on" means on the borrowing side.
    let peers_path = home.path().join("tcr-peers.json");
    let file = crate::peer::config::PeerFile {
        peers: vec![crate::peer::config::PeerRow {
            node: tcr_peer_wire::PeerId([5_u8; 32]),
            label: "lending-mac".to_string(),
            endpoints: vec![crate::peer::config::Endpoint::direct(
                lender_addr,
                0,
                crate::peer::config::EndpointSource::Paired,
            )],
            added_at: 0,
            rendezvous_secret: None,
            sees_us_at: None,
            allow: crate::peer::config::Allow {
                relay: false,
                gateway: false,
                carry: false,
                inspect: false,
                allow_disclose: true,
                accept_move: false,
                control: crate::peer::config::ControlGrants::default(),
            },
            lend: Vec::new(),
        }],
        ..crate::peer::config::PeerFile::default()
    };
    crate::peer::config::save(&peers_path, &file).expect("the peers file writes");

    let config: Config = serde_json::from_str(r#"{ "proxy": { "port": 0 }, "accounts": [] }"#)
        .expect("the inline test config parses");
    let mut handle = serve(ServeOptions {
        persist_path: Some(config_path),
        tls: TlsSetup::Disabled,
        ..ServeOptions::new(config)
    })
    .await
    .expect("bind must succeed on an ephemeral port")
    .expect_started();

    let provider = crate::peer::serve::fallback::configured_provider()
        .expect("the boot sequence installed a peer-lease provider");
    assert_eq!(
        provider.name(),
        "peer-lease",
        "the installed provider is the peer-lease one"
    );

    // One POST at a fleet with no account that can serve it: the dry-fleet
    // arm, which is the only condition under which a provider is consulted.
    let client = crate::base::http_client::builder()
        .no_proxy()
        .build()
        .expect("a loopback client");
    let answered = client
        .post(format!("http://{}/v1/messages", handle.addr()))
        .header("content-type", "application/json")
        .body(r#"{"model":"claude-sonnet-4-5","messages":[]}"#)
        .send()
        .await
        .expect("the proxy answered");
    assert_eq!(
        answered.status().as_u16(),
        429,
        "the fake lender hung up, so the ladder's last rung answers, the honest 429"
    );
    assert_eq!(
        dialled.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the dry-fleet arm reached the installed provider, which dialled the lender \
             named in the peers file beside the config"
    );

    handle.shutdown().await;
}

/// The resolver's own three answers, without a server.
///
/// A caller with no config path reads no peers file at all, which is the
/// inertness every sibling side effect in [`ServeOptions`] has; a caller
/// with one gets the peers file in that config's directory and nowhere
/// else, so `--config` selects a whole profile rather than pairing one
/// profile's accounts with the operator's real trust relationships.
#[test]
fn the_peers_file_is_the_one_beside_the_config_and_nothing_without_one() {
    assert_eq!(peers_file_beside_config(None), None);

    let config = PathBuf::from("/tmp/tcr-unit-profile").join(crate::base::paths::CONFIG_FILE);
    let resolved =
        peers_file_beside_config(Some(&config)).expect("a config path resolves a peers path");
    assert_eq!(
        resolved,
        PathBuf::from("/tmp/tcr-unit-profile/tcr-peers.json"),
        "the peers file is the one beside the config"
    );

    // And it is the same NAME the rest of the peer surface uses, read off
    // that function rather than spelled twice.
    assert_eq!(
        resolved.file_name(),
        crate::peer::config::default_path().file_name()
    );
}

/// Serializes every test below that reads [`crate::peer::tunnel::path_meter`]'s
/// full row set.
///
/// [`crate::peer::tunnel::PathMeter::rows`] returns every (peer, path)
/// the PROCESS has ever charged, not one test's own, because that is
/// exactly the process-wide meter a real serving process has exactly one
/// of. Two of these tests charging it and reading it back concurrently
/// would see each other's rows; this lock makes the process-wide meter
/// behave, for the length of one test, the way it behaves in a real
/// process with exactly one path-traffic task.
fn path_traffic_test_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

/// [`path_traffic_tick`] drives the production writer: a charge on the
/// process-global meter reaches a temp state file after one call.
///
/// Watch it fail by commenting out this test's own call to
/// [`path_traffic_tick`] below: the state file then never exists, since
/// nothing else in this test writes it. That is the same failure a
/// commented-out `save_path_traffic` call inside `path_traffic_tick`
/// itself would produce, which is the one this test exists to catch: see
/// this unit's report for the forced-red run against the real function
/// body.
#[test]
fn path_traffic_tick_writes_a_real_charge_to_the_state_file() {
    let _guard = path_traffic_test_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = std::env::temp_dir().join(format!(
        "tcr-server-path-traffic-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).expect("create the scratch dir");
    let state_path = dir.join("tcr-peer-state.json");

    let peer = tcr_peer_wire::PeerId([9_u8; 32]);
    let path = crate::peer::config::Locator::Direct {
        addr: "127.0.0.1:9321".parse().expect("a socket address"),
    };
    let now_ms = crate::now_ms();
    match crate::peer::tunnel::path_meter().lock() {
        Ok(mut meter) => meter.charge_bytes(&peer, path, 4_096, now_ms),
        Err(_) => panic!("the path meter's lock is poisoned before this test could charge it"),
    }

    let mut last_written: Vec<crate::peer::state::PathTraffic> = Vec::new();
    path_traffic_tick(&state_path, &mut last_written);

    let state = crate::peer::state::load(&state_path, now_ms).expect("read the state file");
    let row = state
        .path_traffic
        .iter()
        .find(|row| row.peer == peer && row.locator == path)
        .expect("the charge landed as a row");
    assert_eq!(row.bytes_last_hour, 4_096, "the charge is the row's figure");
    assert!(
        last_written
            .iter()
            .any(|row| row.peer == peer && row.locator == path && row.bytes_last_hour == 4_096),
        "the tick records what it wrote, for the next wake's comparison: {last_written:?}"
    );
}

/// A second tick with nothing new charged does not touch the file: the
/// mtime is untouched, which is what [`path_traffic_rows_match`] exists to
/// buy.
///
/// Watch it fail by deleting the `path_traffic_rows_match` guard from
/// [`path_traffic_tick`]: the second tick then writes again, moving the
/// mtime for no change in the rows.
#[test]
fn a_second_tick_with_nothing_new_does_not_move_the_files_mtime() {
    let _guard = path_traffic_test_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = std::env::temp_dir().join(format!(
        "tcr-server-path-traffic-quiet-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).expect("create the scratch dir");
    let state_path = dir.join("tcr-peer-state.json");

    let peer = tcr_peer_wire::PeerId([10_u8; 32]);
    let path = crate::peer::config::Locator::Direct {
        addr: "127.0.0.1:9322".parse().expect("a socket address"),
    };
    let now_ms = crate::now_ms();
    match crate::peer::tunnel::path_meter().lock() {
        Ok(mut meter) => meter.charge_bytes(&peer, path, 1_024, now_ms),
        Err(_) => panic!("the path meter's lock is poisoned before this test could charge it"),
    }

    let mut last_written: Vec<crate::peer::state::PathTraffic> = Vec::new();
    path_traffic_tick(&state_path, &mut last_written);
    let first_write = std::fs::metadata(&state_path)
        .expect("the first tick wrote the file")
        .modified()
        .expect("the file has an mtime");

    std::thread::sleep(Duration::from_millis(20));
    path_traffic_tick(&state_path, &mut last_written);
    let second_write = std::fs::metadata(&state_path)
        .expect("the file still exists")
        .modified()
        .expect("the file has an mtime");

    assert_eq!(
        first_write, second_write,
        "the second tick charged nothing new, so it must not have written again"
    );
}
