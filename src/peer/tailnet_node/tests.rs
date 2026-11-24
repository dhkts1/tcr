use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use tokio::io::DuplexStream;
use tokio::sync::mpsc;

use super::*;
use crate::peer::config::{PeerFile, TailnetMode};
use crate::peer::state::{TailnetRecord, TailnetState};

/// Where the fake device hands the far end of each stream it dials.
type DialledTx = mpsc::UnboundedSender<(SocketAddr, DuplexStream)>;

/// What the fake device does, shared with the test that drives it.
#[derive(Clone)]
struct Script {
    hangs: Arc<AtomicUsize>,
    authorized: Arc<AtomicBool>,
    build_fails: Arc<AtomicBool>,
    builds: Arc<AtomicUsize>,
    shutdowns: Arc<AtomicUsize>,
    auth_keys_seen: Arc<AtomicUsize>,
    incoming: Arc<Mutex<Option<mpsc::Receiver<Accepted<DuplexStream>>>>>,
    /// While set, a build waits for `release` before it answers.
    build_hangs: Arc<AtomicBool>,
    release: Arc<tokio::sync::Notify>,
    /// A key file whose existence each shutdown records in
    /// `key_existed_at_shutdown`.
    watch_key: Arc<Mutex<Option<PathBuf>>>,
    key_existed_at_shutdown: Arc<AtomicBool>,
    dialled: Arc<Mutex<Option<DialledTx>>>,
    /// While set, a connect never answers (a dial stuck in flight).
    connect_hangs: Arc<AtomicBool>,
}

impl Script {
    fn new() -> Self {
        Self {
            hangs: Arc::new(AtomicUsize::new(0)),
            authorized: Arc::new(AtomicBool::new(true)),
            build_fails: Arc::new(AtomicBool::new(false)),
            builds: Arc::new(AtomicUsize::new(0)),
            shutdowns: Arc::new(AtomicUsize::new(0)),
            auth_keys_seen: Arc::new(AtomicUsize::new(0)),
            incoming: Arc::new(Mutex::new(None)),
            build_hangs: Arc::new(AtomicBool::new(false)),
            release: Arc::new(tokio::sync::Notify::new()),
            watch_key: Arc::new(Mutex::new(None)),
            key_existed_at_shutdown: Arc::new(AtomicBool::new(false)),
            dialled: Arc::new(Mutex::new(None)),
            connect_hangs: Arc::new(AtomicBool::new(false)),
        }
    }
}

struct FakeFactory(Script);
struct FakeDevice(Script);
struct FakeListener(tokio::sync::Mutex<Option<mpsc::Receiver<Accepted<DuplexStream>>>>);

impl TailnetListener for FakeListener {
    type Stream = DuplexStream;

    async fn accept(&self) -> Result<Accepted<DuplexStream>> {
        let mut incoming = self.0.lock().await;
        match incoming.as_mut() {
            Some(rx) => match rx.recv().await {
                Some(accepted) => Ok(accepted),
                None => std::future::pending().await,
            },
            None => std::future::pending().await,
        }
    }
}

impl TailnetDevice for FakeDevice {
    type Stream = DuplexStream;
    type Listener = FakeListener;

    async fn authorization(&self) -> Result<Authorization> {
        Ok(if self.0.authorized.load(Ordering::SeqCst) {
            Authorization::Authorized
        } else {
            Authorization::NotAuthorized
        })
    }

    async fn self_node(&self) -> Result<NodeSelf> {
        if self
            .0
            .hangs
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            // What a device whose control task panicked looks like: the
            // call never returns. The supervisor's timeout is the test.
            std::future::pending::<()>().await;
        }
        Ok(NodeSelf {
            address: Ipv4Addr::new(100, 64, 0, 7),
            name: "tcr-studio".to_string(),
        })
    }

    async fn listen(&self, _addr: SocketAddr) -> Result<FakeListener> {
        let rx = match self.0.incoming.lock() {
            Ok(mut held) => held.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        Ok(FakeListener(tokio::sync::Mutex::new(rx)))
    }

    async fn shutdown(self, _deadline: Duration) -> bool {
        let watched = match self.0.watch_key.lock() {
            Ok(held) => held.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        if let Some(path) = watched {
            self.0
                .key_existed_at_shutdown
                .store(path.exists(), Ordering::SeqCst);
        }
        self.0.shutdowns.fetch_add(1, Ordering::SeqCst);
        true
    }

    /// One end of a fresh pipe; the other goes to the test through
    /// `dialled`, with the address it was dialled at.
    async fn connect(&self, addr: SocketAddr) -> Result<DuplexStream> {
        if self.0.connect_hangs.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        let (ours, theirs) = tokio::io::duplex(64 * 1024);
        let sender = match self.0.dialled.lock() {
            Ok(held) => held.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        let Some(sender) = sender else {
            bail!("nothing listens at {addr}");
        };
        sender
            .send((addr, theirs))
            .map_err(|_| anyhow!("the test stopped listening"))?;
        Ok(ours)
    }
}

impl DeviceFactory for FakeFactory {
    type Device = FakeDevice;
    const SUPPORTED: bool = true;

    fn mint_keys(&self) -> Result<serde_json::Value> {
        Ok(serde_json::json!({ "fake": "keys" }))
    }

    async fn build(&self, _keys: &NodeKeyFile, auth_key: Option<AuthKey>) -> Result<FakeDevice> {
        self.0.builds.fetch_add(1, Ordering::SeqCst);
        if auth_key.is_some() {
            self.0.auth_keys_seen.fetch_add(1, Ordering::SeqCst);
        }
        if self.0.build_hangs.load(Ordering::SeqCst) {
            self.0.release.notified().await;
        }
        if self.0.build_fails.load(Ordering::SeqCst) {
            bail!("no network");
        }
        Ok(FakeDevice(self.0.clone()))
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    paths: NodePaths,
}

fn fixture(mode: TailnetMode) -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let peers = dir.path().join("tcr-peers.json");
    crate::peer::config::save(
        &peers,
        &PeerFile {
            tailnet: mode.into(),
            ..Default::default()
        },
    )
    .expect("seed the peers file");
    let paths = NodePaths {
        key: key_path_for_peers(&peers),
        state: crate::peer::state::peer_state_path(&peers),
        peers,
    };
    Fixture { _dir: dir, paths }
}

fn some_keys() -> NodeKeyFile {
    NodeKeyFile {
        key_state: serde_json::json!({ "fake": "keys" }),
        hostname: Some("tcr-studio".to_string()),
    }
}

fn set_mode(paths: &NodePaths, mode: TailnetMode) {
    let mut file = crate::peer::config::read_or_default(&paths.peers).expect("read");
    file.tailnet = mode.into();
    crate::peer::config::save(&paths.peers, &file).expect("save");
}

fn held(paths: &NodePaths) -> TailnetRecord {
    crate::peer::state::load(&paths.state, crate::now_ms())
        .expect("load the state file")
        .tailnet
        .expect("a tailnet record")
}

fn supervisor<F: DeviceFactory>(
    factory: F,
    paths: &NodePaths,
    gateway: GatewayFn,
) -> Supervisor<F> {
    let serve: ServeFn<<F::Device as TailnetDevice>::Stream> =
        Arc::new(|_accepted| Box::pin(async {}));
    Supervisor::new(factory, paths.clone(), 7755, serve, gateway)
        .with_control_timeout(Duration::from_millis(100))
        .with_shutdown_deadline(Duration::from_millis(200))
}

fn fixed_gateway() -> GatewayFn {
    Arc::new(|| Some(Ipv4Addr::new(192, 168, 1, 1)))
}

/// **The key file is written 0600, refused at any other mode, and never
/// written over.**
///
/// Watch it fail by removing the mode check in `read_key_file`: the 0644
/// file is then read and the `expect_err` fails.
#[test]
fn the_key_file_is_0600_and_never_overwritten() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = key_path(dir.path());
    write_key_file(&path, &some_keys()).expect("write");
    let mode = std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "written 0600");
    let read = read_key_file(&path).expect("read").expect("present");
    assert_eq!(read.hostname.as_deref(), Some("tcr-studio"));
    assert_eq!(read.key_state, serde_json::json!({ "fake": "keys" }));

    write_key_file(&path, &some_keys()).expect_err("an existing key file is never replaced");

    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    let refusal = read_key_file(&path)
        .expect_err("a 0644 key file is refused")
        .to_string();
    assert!(refusal.contains("644"), "{refusal}");

    assert!(remove_key_file(&path).expect("delete"));
    assert!(read_key_file(&path).expect("read").is_none());
    assert!(!remove_key_file(&path).expect("delete again"));
}

/// **The auth key never prints.**
#[test]
fn an_auth_key_does_not_print() {
    let key = AuthKey::new("tskey-auth-fake-1111\n").expect("a key");
    assert_eq!(format!("{key:?}"), "AuthKey(..)");
    assert_eq!(key.into_secret(), "tskey-auth-fake-1111");
    AuthKey::new("  \n").expect_err("an empty key is refused");
}

/// **A device that stops answering is shut down and a new one is built
/// from the same keys.**
///
/// Watch it fail by making `check_health` return `true` without asking
/// the device: the second build never happens and `builds` stays 1.
#[tokio::test]
async fn a_device_that_fails_its_health_check_is_rebuilt() {
    let fx = fixture(TailnetMode::Node);
    write_key_file(&fx.paths.key, &some_keys()).expect("keys");
    let script = Script::new();
    let mut sup = supervisor(FakeFactory(script.clone()), &fx.paths, fixed_gateway());

    sup.tick(Duration::ZERO, 1_000).await;
    assert!(sup.is_running());
    assert_eq!(script.builds.load(Ordering::SeqCst), 1);
    let ready = held(&fx.paths);
    assert_eq!(ready.state, TailnetState::Ready);
    assert_eq!(ready.address, Some(Ipv4Addr::new(100, 64, 0, 7)));
    assert_eq!(ready.node_name.as_deref(), Some("tcr-studio"));

    // Healthy at the health wake: nothing is rebuilt.
    sup.tick(HEALTH_EVERY, 2_000).await;
    assert_eq!(script.builds.load(Ordering::SeqCst), 1);

    // Dead (its next self_node hangs), but not yet a health wake: left
    // alone, and nothing has asked it yet.
    script.hangs.store(1, Ordering::SeqCst);
    sup.tick(TICK, 3_000).await;
    assert_eq!(script.builds.load(Ordering::SeqCst), 1);
    assert_eq!(script.hangs.load(Ordering::SeqCst), 1);

    // The health wake: the check times out, the device is shut down and
    // a new one is built, which answers.
    sup.tick(HEALTH_EVERY, 4_000).await;
    assert_eq!(
        script.hangs.load(Ordering::SeqCst),
        0,
        "the check asked the device"
    );
    assert_eq!(
        script.shutdowns.load(Ordering::SeqCst),
        1,
        "the old device was shut down"
    );
    assert_eq!(
        script.builds.load(Ordering::SeqCst),
        2,
        "a new one was built"
    );
    assert!(sup.is_running());
    assert_eq!(held(&fx.paths).state, TailnetState::Ready);
}

/// **A move to another network rebuilds the device at the next health
/// wake, even though the device still answers.**
///
/// Watch it fail by dropping the `moved` branch in `check_health`.
#[tokio::test]
async fn another_network_rebuilds_the_device() {
    let fx = fixture(TailnetMode::Node);
    write_key_file(&fx.paths.key, &some_keys()).expect("keys");
    let script = Script::new();
    let router = Arc::new(Mutex::new(Some(Ipv4Addr::new(192, 168, 1, 1))));
    let read = Arc::clone(&router);
    let gateway: GatewayFn = Arc::new(move || match read.lock() {
        Ok(held) => *held,
        Err(poisoned) => *poisoned.into_inner(),
    });
    let mut sup = supervisor(FakeFactory(script.clone()), &fx.paths, gateway);

    sup.tick(Duration::ZERO, 1_000).await;
    sup.tick(HEALTH_EVERY, 2_000).await;
    assert_eq!(
        script.builds.load(Ordering::SeqCst),
        1,
        "same network, no rebuild"
    );

    match router.lock() {
        Ok(mut held) => *held = Some(Ipv4Addr::new(10, 0, 0, 1)),
        Err(poisoned) => *poisoned.into_inner() = Some(Ipv4Addr::new(10, 0, 0, 1)),
    }
    sup.tick(TICK, 3_000).await;
    assert_eq!(
        script.builds.load(Ordering::SeqCst),
        1,
        "not a health wake yet"
    );
    sup.tick(HEALTH_EVERY, 4_000).await;
    assert_eq!(script.shutdowns.load(Ordering::SeqCst), 1);
    assert_eq!(script.builds.load(Ordering::SeqCst), 2);
}

/// **The record names each node state**: not joined without a key file,
/// connecting while a build fails, not joined when control refuses the
/// keys, ready on the tailnet, and off once the mode is.
///
/// Watch it fail by recording `NodeConnecting` for a missing key file.
#[tokio::test]
async fn the_record_names_each_node_state() {
    let fx = fixture(TailnetMode::Node);
    let script = Script::new();
    let mut sup = supervisor(FakeFactory(script.clone()), &fx.paths, fixed_gateway());

    sup.tick(Duration::ZERO, 1_000).await;
    let record = held(&fx.paths);
    assert_eq!(record.mode.effective(), TailnetMode::Node);
    assert_eq!(record.state, TailnetState::NodeNotJoined);
    assert_eq!(
        script.builds.load(Ordering::SeqCst),
        0,
        "no keys, no device"
    );

    write_key_file(&fx.paths.key, &some_keys()).expect("keys");
    script.build_fails.store(true, Ordering::SeqCst);
    sup.tick(TICK, 2_000).await;
    let record = held(&fx.paths);
    assert_eq!(record.state, TailnetState::NodeConnecting);
    assert!(
        record
            .detail
            .as_deref()
            .is_some_and(|d| d.contains("no network")),
        "{record:?}"
    );
    // Backed off: the next wake does not build again.
    sup.tick(TICK, 3_000).await;
    assert_eq!(script.builds.load(Ordering::SeqCst), 1);

    script.build_fails.store(false, Ordering::SeqCst);
    script.authorized.store(false, Ordering::SeqCst);
    sup.tick(RETRY_AFTER_FAILURE, 4_000).await;
    let record = held(&fx.paths);
    assert_eq!(record.state, TailnetState::NodeNotJoined, "{record:?}");
    assert_eq!(
        script.shutdowns.load(Ordering::SeqCst),
        1,
        "the refused device is shut down"
    );

    script.authorized.store(true, Ordering::SeqCst);
    // The second failure in a row: the refusal's 300 s rung doubled, at the cap.
    sup.tick(RETRY_AFTER_REFUSAL, 5_000).await;
    assert_eq!(
        held(&fx.paths).state,
        TailnetState::NodeNotJoined,
        "still backing off"
    );
    sup.tick(RETRY_CAP.saturating_sub(RETRY_AFTER_REFUSAL), 5_000)
        .await;
    let record = held(&fx.paths);
    assert_eq!(record.state, TailnetState::Ready);
    assert_eq!(record.recorded_at_ms, 5_000);

    set_mode(&fx.paths, TailnetMode::Off);
    sup.tick(TICK, 6_000).await;
    assert!(!sup.is_running());
    assert_eq!(script.shutdowns.load(Ordering::SeqCst), 2);
    let record = held(&fx.paths);
    assert_eq!(
        (record.mode, record.state),
        (TailnetMode::Off.into(), TailnetState::NodeReleased)
    );
}

/// **A Mac that never used node mode never has this supervisor write the
/// state file**, in any build: off and app records are the browse
/// tick's.
///
/// Watch it fail by dropping `was_node &&` from the off branch: the first
/// wake then writes an `off` record.
#[tokio::test]
async fn outside_node_mode_nothing_is_written() {
    for mode in [TailnetMode::Off, TailnetMode::App] {
        let fx = fixture(mode);
        let mut sup = supervisor(FakeFactory(Script::new()), &fx.paths, fixed_gateway());
        sup.tick(Duration::ZERO, 1_000).await;
        sup.tick(TICK, 2_000).await;
        assert!(!fx.paths.state.exists(), "{mode:?} wrote the state file");
        let mut unsupported = supervisor(Unsupported, &fx.paths, fixed_gateway());
        unsupported.tick(Duration::ZERO, 3_000).await;
        assert!(!fx.paths.state.exists(), "{mode:?} wrote the state file");
    }
}

/// **A binary without the library records `nodeUnsupported`** in node
/// mode and builds nothing.
#[tokio::test]
async fn a_build_without_the_library_records_unsupported() {
    let fx = fixture(TailnetMode::Node);
    write_key_file(&fx.paths.key, &some_keys()).expect("keys");
    let mut sup = supervisor(Unsupported, &fx.paths, fixed_gateway());
    sup.tick(Duration::ZERO, 1_000).await;
    assert!(!sup.is_running());
    assert_eq!(held(&fx.paths).state, TailnetState::NodeUnsupported);
}

/// **The node's accept loop hands each connection to the same serve
/// function, with the node's addresses, and counts it while it is open.**
#[tokio::test]
async fn accepted_connections_reach_serve_with_their_addresses() {
    let fx = fixture(TailnetMode::Node);
    write_key_file(&fx.paths.key, &some_keys()).expect("keys");
    let script = Script::new();
    let (tx, rx) = mpsc::channel(1);
    match script.incoming.lock() {
        Ok(mut held) => *held = Some(rx),
        Err(poisoned) => *poisoned.into_inner() = Some(rx),
    }
    let (seen_tx, mut seen_rx) = mpsc::channel::<(SocketAddr, SocketAddr)>(1);
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let release = Arc::new(Mutex::new(Some(release_rx)));
    let serve: ServeFn<DuplexStream> = Arc::new(move |accepted| {
        let seen_tx = seen_tx.clone();
        let release = match release.lock() {
            Ok(mut held) => held.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        Box::pin(async move {
            seen_tx
                .send((accepted.from, accepted.bind))
                .await
                .expect("the test listens");
            if let Some(release) = release {
                release.await.expect("released");
            }
        })
    });
    let mut sup = Supervisor::new(
        FakeFactory(script.clone()),
        fx.paths.clone(),
        7755,
        serve,
        fixed_gateway(),
    );
    sup.tick(Duration::ZERO, 1_000).await;
    let live = sup.live();

    let from: SocketAddr = "100.64.0.9:50123".parse().expect("addr");
    let bind: SocketAddr = "100.64.0.7:7755".parse().expect("addr");
    let (stream, _far) = tokio::io::duplex(64);
    tx.send(Accepted { stream, from, bind })
        .await
        .expect("accept");
    let got = seen_rx.recv().await.expect("served");
    assert_eq!(got, (from, bind));
    assert_eq!(live.distinct(), 1, "counted while open");
    release_tx.send(()).expect("release");
    for _ in 0..100 {
        if live.distinct() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(live.distinct(), 0, "uncounted once it ends");
}

/// **A join writes the key file only after control accepts the auth key,
/// and the mode only after that; a refused key leaves both alone.**
///
/// Watch it fail by moving the mode write above the authorization check:
/// the refused join then leaves `tailnet: "node"` behind.
#[tokio::test]
async fn a_join_saves_keys_then_mode_and_a_refusal_saves_nothing() {
    use std::os::unix::fs::PermissionsExt as _;

    let fx = fixture(TailnetMode::Off);
    let script = Script::new();
    let factory = FakeFactory(script.clone());

    script.authorized.store(false, Ordering::SeqCst);
    let refused = join_with(
        &factory,
        &fx.paths.peers,
        &fx.paths.key,
        Some("tcr-studio".to_string()),
        AuthKey::new("tskey-auth-fake-1111").expect("key"),
        Duration::from_secs(5),
    )
    .await
    .expect_err("a refused key is an error")
    .to_string();
    assert!(refused.contains("did not accept"), "{refused}");
    assert!(read_key_file(&fx.paths.key).expect("read").is_none());
    assert_eq!(
        crate::peer::config::read_or_default(&fx.paths.peers)
            .expect("read")
            .tailnet
            .effective(),
        TailnetMode::Off
    );
    assert_eq!(
        script.shutdowns.load(Ordering::SeqCst),
        1,
        "the device was shut down"
    );

    script.authorized.store(true, Ordering::SeqCst);
    let me = join_with(
        &factory,
        &fx.paths.peers,
        &fx.paths.key,
        Some("tcr-studio".to_string()),
        AuthKey::new("tskey-auth-fake-1111").expect("key"),
        Duration::from_secs(5),
    )
    .await
    .expect("joined");
    assert_eq!(me.address, Ipv4Addr::new(100, 64, 0, 7));
    assert_eq!(script.auth_keys_seen.load(Ordering::SeqCst), 2);
    assert_eq!(script.shutdowns.load(Ordering::SeqCst), 2);
    let keys = read_key_file(&fx.paths.key)
        .expect("read")
        .expect("written");
    assert_eq!(keys.hostname.as_deref(), Some("tcr-studio"));
    let mode = std::fs::metadata(&fx.paths.key)
        .expect("meta")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
    assert_eq!(
        crate::peer::config::read_or_default(&fx.paths.peers)
            .expect("read")
            .tailnet
            .effective(),
        TailnetMode::Node
    );
    let raw = std::fs::read_to_string(&fx.paths.key).expect("raw");
    assert!(!raw.contains("tskey"), "the auth key is never written");

    join_with(
        &factory,
        &fx.paths.peers,
        &fx.paths.key,
        None,
        AuthKey::new("tskey-auth-fake-1111").expect("key"),
        Duration::from_secs(5),
    )
    .await
    .expect_err("a joined node is refused a second join");
}

/// **A binary without the library refuses to join.**
#[tokio::test]
async fn a_build_without_the_library_refuses_to_join() {
    let fx = fixture(TailnetMode::Off);
    let refusal = join_with(
        &Unsupported,
        &fx.paths.peers,
        &fx.paths.key,
        None,
        AuthKey::new("tskey-auth-fake-1111").expect("key"),
        Duration::from_secs(5),
    )
    .await
    .expect_err("refused")
    .to_string();
    assert!(refusal.contains("tailnet-node"), "{refusal}");
}

fn save_held(paths: &NodePaths, mode: TailnetMode, state: TailnetState) {
    crate::peer::state::save_tailnet(
        &paths.state,
        TailnetRecord {
            mode: mode.into(),
            state,
            address: None,
            node_name: None,
            found: 0,
            connected: 0,
            detail: None,
            recorded_at_ms: crate::now_ms(),
        },
        Vec::new(),
    )
    .expect("save the record");
}

/// **`leave` sets the mode off first, keeps the key file while the server
/// still holds the node, is not fooled by another writer's `off` record,
/// and deletes the keys once the supervisor writes `nodeReleased`.**
///
/// Watch it fail two ways: delete the key file before the wait (the
/// mid-wait assertion fails), or accept `mode == off` as released again
/// (the keys go on the browse tick's `off` record).
#[tokio::test]
async fn leave_waits_for_the_server_before_deleting_the_keys() {
    let fx = fixture(TailnetMode::Node);
    write_key_file(&fx.paths.key, &some_keys()).expect("keys");
    save_held(&fx.paths, TailnetMode::Node, TailnetState::Ready);

    let peers = fx.paths.peers.clone();
    let key = fx.paths.key.clone();
    let leaving = tokio::spawn(async move {
        leave_with(
            &peers,
            &key,
            Duration::from_secs(10),
            Duration::from_millis(20),
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        crate::peer::config::read_or_default(&fx.paths.peers)
            .expect("read")
            .tailnet
            .effective(),
        TailnetMode::Off,
        "the mode goes off first"
    );
    assert!(
        read_key_file(&fx.paths.key).expect("read").is_some(),
        "the keys stay while the server holds the node"
    );
    assert!(!leaving.is_finished());

    // The browse tick's own `off` record: not the node letting go.
    save_held(&fx.paths, TailnetMode::Off, TailnetState::Off);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        read_key_file(&fx.paths.key).expect("read").is_some(),
        "an `off` record from another writer does not release the keys"
    );
    assert!(!leaving.is_finished());

    save_held(&fx.paths, TailnetMode::Off, TailnetState::NodeReleased);
    let outcome = leaving.await.expect("joined").expect("left");
    assert_eq!(outcome, LeaveOutcome::Deleted);
    assert!(read_key_file(&fx.paths.key).expect("read").is_none());
}

/// **A server that never lets go keeps the key file**, and with no server
/// recording at all, `leave` does not wait.
#[tokio::test]
async fn leave_keeps_the_keys_when_the_server_does_not_let_go() {
    let fx = fixture(TailnetMode::Node);
    write_key_file(&fx.paths.key, &some_keys()).expect("keys");
    save_held(&fx.paths, TailnetMode::Node, TailnetState::Ready);
    leave_with(
        &fx.paths.peers,
        &fx.paths.key,
        Duration::from_millis(100),
        Duration::from_millis(20),
    )
    .await
    .expect_err("the server still holds the node");
    assert!(read_key_file(&fx.paths.key).expect("read").is_some());

    let quiet = fixture(TailnetMode::Node);
    write_key_file(&quiet.paths.key, &some_keys()).expect("keys");
    let outcome = leave_with(
        &quiet.paths.peers,
        &quiet.paths.key,
        Duration::from_millis(100),
        Duration::from_millis(20),
    )
    .await
    .expect("no server, no wait");
    assert_eq!(outcome, LeaveOutcome::Deleted);
}

fn switchable_gateway() -> (Arc<Mutex<Option<Ipv4Addr>>>, GatewayFn) {
    let router = Arc::new(Mutex::new(Some(Ipv4Addr::new(192, 168, 1, 1))));
    let read = Arc::clone(&router);
    let gateway: GatewayFn = Arc::new(move || match read.lock() {
        Ok(held) => *held,
        Err(poisoned) => *poisoned.into_inner(),
    });
    (router, gateway)
}

fn move_to(router: &Arc<Mutex<Option<Ipv4Addr>>>, to: Ipv4Addr) {
    match router.lock() {
        Ok(mut held) => *held = Some(to),
        Err(poisoned) => *poisoned.into_inner() = Some(to),
    }
}

/// **No second device is built while the old one is still alive.** A
/// caller holding the device past the shutdown deadline (a dial in
/// flight) leaves it running; the rebuild waits until it is free, shuts
/// it down, and only then builds from the same keys.
///
/// Watch it fail by dropping the held device in `settle` instead of
/// keeping it in `lingering`: the rebuild then happens with `shutdowns`
/// still 0, two devices on one key.
#[tokio::test]
async fn a_rebuild_waits_for_the_old_device_to_stop() {
    let fx = fixture(TailnetMode::Node);
    write_key_file(&fx.paths.key, &some_keys()).expect("keys");
    let script = Script::new();
    let (router, gateway) = switchable_gateway();
    let mut sup = supervisor(FakeFactory(script.clone()), &fx.paths, gateway);
    sup.tick(Duration::ZERO, 1_000).await;
    let in_flight = sup.running_device().expect("running");

    move_to(&router, Ipv4Addr::new(10, 0, 0, 1));
    sup.tick(HEALTH_EVERY, 2_000).await;
    assert_eq!(script.shutdowns.load(Ordering::SeqCst), 0, "still held");
    assert_eq!(
        script.builds.load(Ordering::SeqCst),
        1,
        "no second device while the first is alive"
    );
    let record = held(&fx.paths);
    assert_eq!(record.state, TailnetState::NodeConnecting);
    assert!(
        record
            .detail
            .as_deref()
            .is_some_and(|d| d.contains("still in use")),
        "{record:?}"
    );

    sup.tick(RETRY_AFTER_FAILURE, 3_000).await;
    assert_eq!(script.builds.load(Ordering::SeqCst), 1, "still held");

    drop(in_flight);
    sup.tick(TICK, 4_000).await;
    assert_eq!(
        script.shutdowns.load(Ordering::SeqCst),
        1,
        "shut down once free"
    );
    assert_eq!(
        script.builds.load(Ordering::SeqCst),
        2,
        "then the new one is built"
    );
    assert!(sup.is_running());
}

/// **The release record is written on any way out of `node`, and only
/// once no device from the keys is alive**: leaving for `app` while a
/// caller still holds the device writes nothing, and the release follows
/// the shutdown.
///
/// Watch it fail by writing the release whenever the mode leaves `node`
/// (drop `free && built`): it is then written while the device lingers.
#[tokio::test]
async fn the_release_waits_for_the_device_and_covers_app_mode() {
    let fx = fixture(TailnetMode::Node);
    write_key_file(&fx.paths.key, &some_keys()).expect("keys");
    let script = Script::new();
    let mut sup = supervisor(FakeFactory(script.clone()), &fx.paths, fixed_gateway());
    sup.tick(Duration::ZERO, 1_000).await;
    let in_flight = sup.running_device().expect("running");

    set_mode(&fx.paths, TailnetMode::App);
    sup.tick(TICK, 2_000).await;
    assert_eq!(script.shutdowns.load(Ordering::SeqCst), 0);
    assert_eq!(
        held(&fx.paths).state,
        TailnetState::Ready,
        "nothing released while the device is alive"
    );

    drop(in_flight);
    sup.tick(TICK, 3_000).await;
    assert_eq!(script.shutdowns.load(Ordering::SeqCst), 1);
    let record = held(&fx.paths);
    assert_eq!(
        (record.mode, record.state, record.recorded_at_ms),
        (TailnetMode::App.into(), TailnetState::NodeReleased, 3_000)
    );
}

/// The ladder: 30 s, doubling, capped at ten minutes.
#[test]
fn the_retry_ladder_doubles_to_ten_minutes() {
    let waits: Vec<u64> = (1..=7)
        .map(|n| retry_wait(RETRY_AFTER_FAILURE, n).as_secs())
        .collect();
    assert_eq!(waits, vec![30, 60, 120, 240, 480, 600, 600]);
    assert_eq!(retry_wait(RETRY_AFTER_REFUSAL, 1).as_secs(), 300);
    assert_eq!(retry_wait(RETRY_AFTER_REFUSAL, 2).as_secs(), 600);
    assert_eq!(retry_wait(RETRY_AFTER_FAILURE, u32::MAX).as_secs(), 600);
}

/// **Failed starts back off 30 s, 60 s, 120 s; a device that passes a
/// health check puts the ladder back at its first rung.**
///
/// Watch it fail by making `retry_wait` return `first` every time: the
/// build at 30 s into the second wait then happens.
#[tokio::test]
async fn failed_starts_back_off_and_a_healthy_device_resets_it() {
    let fx = fixture(TailnetMode::Node);
    write_key_file(&fx.paths.key, &some_keys()).expect("keys");
    let script = Script::new();
    let builds = || script.builds.load(Ordering::SeqCst);
    let mut sup = supervisor(FakeFactory(script.clone()), &fx.paths, fixed_gateway());

    script.build_fails.store(true, Ordering::SeqCst);
    sup.tick(Duration::ZERO, 1_000).await;
    assert_eq!(builds(), 1);
    sup.tick(Duration::from_secs(29), 2_000).await;
    assert_eq!(builds(), 1, "inside the first 30 s");
    sup.tick(Duration::from_secs(1), 3_000).await;
    assert_eq!(builds(), 2, "after 30 s");
    sup.tick(Duration::from_secs(30), 4_000).await;
    assert_eq!(builds(), 2, "the second wait is 60 s, not 30");
    sup.tick(Duration::from_secs(30), 5_000).await;
    assert_eq!(builds(), 3, "after 60 s");
    sup.tick(Duration::from_secs(119), 6_000).await;
    assert_eq!(builds(), 3, "the third wait is 120 s");

    script.build_fails.store(false, Ordering::SeqCst);
    sup.tick(Duration::from_secs(1), 7_000).await;
    assert_eq!(builds(), 4);
    assert!(sup.is_running());
    // A passed health check: the ladder is back at its first rung.
    sup.tick(HEALTH_EVERY, 8_000).await;
    // It dies; the first failure rebuilds at once, and that build fails.
    script.build_fails.store(true, Ordering::SeqCst);
    script.hangs.store(1, Ordering::SeqCst);
    sup.tick(HEALTH_EVERY, 9_000).await;
    assert_eq!(
        builds(),
        5,
        "the first failure after a healthy run rebuilds at once"
    );
    sup.tick(Duration::from_secs(59), 10_000).await;
    assert_eq!(builds(), 5);
    sup.tick(Duration::from_secs(1), 11_000).await;
    assert_eq!(
        builds(),
        6,
        "60 s, the second rung: the count restarted, it did not carry the old three"
    );
}

/// **A build that outlives its wait is kept and waited on again, never
/// started a second time**: an offline Mac holds one half-started build,
/// not one per retry. When it finishes, the next start uses it.
///
/// Watch it fail by spawning a fresh build on every start (ignore
/// `pending_build`): `builds` then reaches 2 while the first still runs.
#[tokio::test]
async fn a_slow_build_is_kept_not_repeated() {
    let fx = fixture(TailnetMode::Node);
    write_key_file(&fx.paths.key, &some_keys()).expect("keys");
    let script = Script::new();
    script.build_hangs.store(true, Ordering::SeqCst);
    let mut sup = supervisor(FakeFactory(script.clone()), &fx.paths, fixed_gateway())
        .with_build_timeout(Duration::from_millis(50));

    sup.tick(Duration::ZERO, 1_000).await;
    assert!(!sup.is_running());
    assert_eq!(held(&fx.paths).state, TailnetState::NodeConnecting);
    sup.tick(RETRY_AFTER_FAILURE, 2_000).await;
    assert_eq!(
        script.builds.load(Ordering::SeqCst),
        1,
        "the second start waits on the first build"
    );

    script.release.notify_one();
    tokio::time::sleep(Duration::from_millis(50)).await;
    sup.tick(RETRY_CAP, 3_000).await;
    assert!(sup.is_running(), "the kept build became the node");
    assert_eq!(script.builds.load(Ordering::SeqCst), 1);
}

/// **Stopping the process does not wait out a start**: the stop is seen
/// during a wake, and `run` returns within the shutdown deadline even
/// with a build that never finishes.
///
/// Watch it fail by awaiting the wake without watching `stop`: `run`
/// then waits the 30 s build ceiling and the 2 s deadline here fails.
#[tokio::test]
async fn a_stop_during_a_start_is_not_kept_waiting() {
    let fx = fixture(TailnetMode::Node);
    write_key_file(&fx.paths.key, &some_keys()).expect("keys");
    let script = Script::new();
    script.build_hangs.store(true, Ordering::SeqCst);
    let sup = supervisor(FakeFactory(script.clone()), &fx.paths, fixed_gateway());
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let running = tokio::spawn(sup.run(Duration::from_millis(10), stop_rx));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        script.builds.load(Ordering::SeqCst),
        1,
        "a build is under way"
    );
    stop_tx.send(true).expect("stop");
    tokio::time::timeout(Duration::from_secs(2), running)
        .await
        .expect("run returns without waiting out the build")
        .expect("joined");
}

/// **A join shuts its device down before the key file exists**, so a
/// server already in node mode cannot start a second device from it
/// while the CLI's is still up.
///
/// Watch it fail by writing the key file before the shutdown: the
/// shutdown then sees the file.
#[tokio::test]
async fn a_join_shuts_its_device_down_before_writing_the_keys() {
    let fx = fixture(TailnetMode::Off);
    let script = Script::new();
    match script.watch_key.lock() {
        Ok(mut held) => *held = Some(fx.paths.key.clone()),
        Err(poisoned) => *poisoned.into_inner() = Some(fx.paths.key.clone()),
    }
    join_with(
        &FakeFactory(script.clone()),
        &fx.paths.peers,
        &fx.paths.key,
        Some("tcr-studio".to_string()),
        AuthKey::new("tskey-auth-fake-1111").expect("key"),
        Duration::from_secs(5),
    )
    .await
    .expect("joined");
    assert_eq!(script.shutdowns.load(Ordering::SeqCst), 1);
    assert!(
        !script.key_existed_at_shutdown.load(Ordering::SeqCst),
        "the key file did not exist yet when the device was shut down"
    );
    assert!(read_key_file(&fx.paths.key).expect("read").is_some());
}

/// **A running node publishes a dialler that reaches tailnet addresses
/// only, and withdraws it when the node stops.** This is the decision
/// `serve::connect_within` and the dial route ask: a LAN address, or no
/// node, goes through the kernel.
///
/// Watch it fail by dropping the `slot.set(None)` in `stop_device`: the
/// last dial then still finds a dialler.
#[tokio::test]
async fn the_dialler_is_published_while_the_node_runs() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let fx = fixture(TailnetMode::Node);
    write_key_file(&fx.paths.key, &some_keys()).expect("keys");
    let script = Script::new();
    let (dialled_tx, mut dialled_rx) = mpsc::unbounded_channel();
    match script.dialled.lock() {
        Ok(mut held) => *held = Some(dialled_tx),
        Err(poisoned) => *poisoned.into_inner() = Some(dialled_tx),
    }
    let slot = DialSlot::default();
    let mut sup = supervisor(FakeFactory(script.clone()), &fx.paths, fixed_gateway())
        .with_dial_slot(slot.clone());
    let tailnet: SocketAddr = "100.64.0.9:7755".parse().expect("addr");
    let lan: SocketAddr = "192.168.1.20:7755".parse().expect("addr");

    assert!(
        dial_in_slot(&slot, tailnet, Duration::from_secs(1))
            .await
            .is_none()
    );
    sup.tick(Duration::ZERO, 1_000).await;
    assert_eq!(
        slot.listen(),
        Some("100.64.0.7:7755".parse().expect("addr"))
    );
    assert!(
        dial_in_slot(&slot, lan, Duration::from_secs(1))
            .await
            .is_none(),
        "a LAN address is the kernel's"
    );
    let mut stream = dial_in_slot(&slot, tailnet, Duration::from_secs(1))
        .await
        .expect("a tailnet address goes through the node")
        .expect("dialled");
    let (addr, mut far) = dialled_rx.recv().await.expect("the node dialled");
    assert_eq!(addr, tailnet);
    stream.write_all(b"hi").await.expect("write");
    let mut got = [0_u8; 2];
    far.read_exact(&mut got).await.expect("read");
    assert_eq!(&got, b"hi");
    assert_eq!(sup.live().distinct(), 1, "a dial counts as connected");
    drop(stream);
    assert_eq!(sup.live().distinct(), 0);

    set_mode(&fx.paths, TailnetMode::Off);
    sup.tick(TICK, 2_000).await;
    assert!(slot.listen().is_none());
    assert!(
        dial_in_slot(&slot, tailnet, Duration::from_secs(1))
            .await
            .is_none()
    );
}

/// **A dial stuck in flight cannot keep the device alive into a
/// rebuild**: stopping the device cancels it, the old device is shut
/// down, and only then is the new one built.
///
/// Watch it fail by not sending the cancel in `stop_device`: the stuck
/// dial keeps the device past its deadline, it lingers, and no rebuild
/// happens on this wake (`builds` stays 1, `shutdowns` 0).
#[tokio::test]
async fn a_dial_in_flight_is_cancelled_before_the_rebuild() {
    let fx = fixture(TailnetMode::Node);
    write_key_file(&fx.paths.key, &some_keys()).expect("keys");
    let script = Script::new();
    script.connect_hangs.store(true, Ordering::SeqCst);
    let (router, gateway) = switchable_gateway();
    let slot = DialSlot::default();
    let mut sup =
        supervisor(FakeFactory(script.clone()), &fx.paths, gateway).with_dial_slot(slot.clone());
    sup.tick(Duration::ZERO, 1_000).await;

    let in_flight = {
        let slot = slot.clone();
        tokio::spawn(async move {
            dial_in_slot(
                &slot,
                "100.64.0.9:7755".parse().expect("addr"),
                Duration::from_secs(30),
            )
            .await
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!in_flight.is_finished(), "the dial is stuck in its connect");

    move_to(&router, Ipv4Addr::new(10, 0, 0, 1));
    sup.tick(HEALTH_EVERY, 2_000).await;
    assert_eq!(
        script.shutdowns.load(Ordering::SeqCst),
        1,
        "the old device was shut down"
    );
    assert_eq!(
        script.builds.load(Ordering::SeqCst),
        2,
        "and only then a new one built"
    );
    let dialled = tokio::time::timeout(Duration::from_secs(1), in_flight)
        .await
        .expect("the stuck dial ended")
        .expect("joined")
        .expect("it went through the node");
    let refusal = format!("{:#}", dialled.err().expect("cancelled, not connected"));
    assert!(refusal.contains("stopped"), "{refusal}");
}

/// A slot whose node counts every dial it is asked for and answers each
/// with a fresh pipe, serving the peers file at `peers`.
fn counting_slot(peers: &Path) -> (DialSlot, Arc<AtomicUsize>) {
    let asked = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&asked);
    let dial: DialFn = Arc::new(move |_addr| {
        counter.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            let (ours, _theirs) = tokio::io::duplex(64);
            Ok(Box::new(ours) as crate::peer::serve::PeerStream)
        })
    });
    let slot = DialSlot::default();
    slot.set(Some(Published {
        dial,
        listen: "100.64.0.7:7755".parse().expect("addr"),
        peers: peers.to_path_buf(),
    }));
    (slot, asked)
}

/// **The dial route reaches peer ports only** (Gil, 2026-10-20): the
/// default 7755, this Mac's own `listen` port, and a port a pinned row
/// carries are dialled; port 22 is refused with a sentence naming it,
/// and the node is never asked.
///
/// Watch it fail by skipping the port check in `dial_for_route_in`: port
/// 22 is then dialled and `asked` reaches 4.
#[tokio::test]
async fn the_dial_route_reaches_peer_ports_only() {
    use crate::peer::config::{Allow, Endpoint, EndpointSource, PeerRow};

    let fx = fixture(TailnetMode::Node);
    let mut file = crate::peer::config::read_or_default(&fx.paths.peers).expect("read");
    file.listen = Some("0.0.0.0:9700".parse().expect("addr"));
    let mut row = PeerRow {
        node: tcr_peer_wire::PeerId([7_u8; 32]),
        label: "laptop-2".to_string(),
        endpoints: Vec::new(),
        added_at: 0,
        allow: Allow::default(),
        lend: Vec::new(),
        rendezvous_secret: None,
        sees_us_at: None,
    };
    row.observe_endpoint(Endpoint::direct(
        "100.64.0.9:9600".parse().expect("addr"),
        1_000,
        EndpointSource::Paired,
    ));
    file.peers.push(row);
    crate::peer::config::save(&fx.paths.peers, &file).expect("save");
    let (slot, asked) = counting_slot(&fx.paths.peers);
    let within = Duration::from_secs(1);

    for port in [7755_u16, 9700, 9600] {
        let target = SocketAddr::from((Ipv4Addr::new(100, 64, 0, 9), port));
        assert!(
            matches!(
                dial_for_route_in(&slot, target, within).await,
                RouteDial::Dialled(_)
            ),
            "port {port} is a peer port"
        );
    }
    assert_eq!(asked.load(Ordering::SeqCst), 3);

    let ssh = SocketAddr::from((Ipv4Addr::new(100, 64, 0, 9), 22));
    let RouteDial::PortRefused(why) = dial_for_route_in(&slot, ssh, within).await else {
        panic!("port 22 must be refused");
    };
    assert!(
        why.contains("port 22") && why.contains("peer port"),
        "{why}"
    );
    assert_eq!(asked.load(Ordering::SeqCst), 3, "the node was never asked");
}

#[test]
fn node_names_are_checked() {
    check_node_name("tcr-studio").expect("ok");
    check_node_name("-x").expect_err("leading hyphen");
    check_node_name("a b").expect_err("space");
    check_node_name("").expect_err("empty");
    check_node_name(&"a".repeat(64)).expect_err("too long");
}
