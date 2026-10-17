//! The harness pieces the `tests/peer_*.rs` files share: scratch directories, a
//! node's key material, a loopback listener, the shipped accept loop, a pinned
//! dial that opens a stream, a counting target, and the two tracing captures.
//!
//! Each of these was copied into three to thirteen files, and the copies had
//! started to drift (a `record_f64` one copy had and its twin did not, an
//! exact-message match in one and a substring match in the other). One copy
//! here is what keeps the instruments the same.
//!
//! Compiled into the `it` crate through `tests/it/main.rs`, and into each
//! separate `[[test]]` that needs it through its own `#[path]` module, always
//! named `peer_support` at the crate root so a file reads `crate::peer_support`
//! wherever it is built. Nothing here installs process-wide state: the
//! captures are layers, and the file that installs one owns that decision.

#![allow(
    dead_code,
    reason = "each test crate uses a different subset of this module"
)]

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tcr::log_err::LogErr as _;
use tcr::peer::config::{self as peer_config, PeerFile, PeerRow};
use tcr::peer::id::NodeKey;
use tcr::peer::listener::{self, SessionContext};
use tcr::peer::noise::{self, Handshake};
use tcr_peer_wire::{PeerId, StreamHeader};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};

/// A fresh scratch directory, `tcr-peer-<file>-<tag>-<pid>-<thread>` under the
/// temp dir, so two tests in one binary and several lanes running at once
/// never collide on one path. Emptied first: a directory left by a killed run
/// must not hand this one its files.
pub fn scratch_dir(file: &str, tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tcr-peer-{file}-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::remove_dir_all(&dir).log_debug("removing a test directory");
    std::fs::create_dir_all(&dir).expect("create the scratch dir");
    dir
}

/// One machine's key material, as a test holds it.
pub struct Node {
    pub secret: [u8; 32],
    pub id: PeerId,
}

/// A freshly minted static keypair.
pub fn node() -> Node {
    let (secret, public) = noise::generate_static().expect("mint a static keypair");
    Node {
        secret,
        id: PeerId(public),
    }
}

/// A listener on a kernel-chosen loopback port.
pub async fn loopback() -> TcpListener {
    TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a loopback listener")
}

/// Bind `127.0.0.1:0`, start the SHIPPED accept loop on it, and report the
/// address the kernel chose.
///
/// `serve_on_with` and not a hand-rolled loop: the caps, the ban check, the
/// pattern dispatch and the knock queue all live in it, so a harness that
/// re-implemented the loop would be measuring the harness.
pub async fn serve_shipped(context: SessionContext) -> SocketAddr {
    let listener = listener::bind("127.0.0.1:0".parse().expect("a loopback address"))
        .await
        .expect("bind a kernel-chosen port on loopback");
    let addr = listener.local_addr().expect("the bound address");
    tokio::spawn(async move {
        // The loop returns only on an accept failure, which is the harness
        // going away at the end of the test.
        listener::serve_on_with(listener, context)
            .await
            .log_debug("serving a test peer listener");
    });
    addr
}

/// Dial `addr` as `requester` with a `Return` handshake pinned to `responder`,
/// and write `header` as the stream's first frame. The stream and session are
/// the caller's for whatever the test sends or reads next.
pub async fn dial_and_open(
    addr: SocketAddr,
    requester: &Node,
    responder: &PeerId,
    header: &StreamHeader,
) -> (TcpStream, noise::PeerSession) {
    let mut stream = TcpStream::connect(addr).await.expect("dial the peer");
    let mut session = noise::dial_handshake(
        &mut stream,
        &requester.secret,
        Handshake::Return,
        Some(&responder.0),
        None,
    )
    .await
    .expect("a pinned peer completes the handshake");
    noise::send_encrypted(
        &mut stream,
        &mut session.transport,
        &serde_json::to_vec(header).expect("the header serializes"),
    )
    .await
    .expect("the header is written");
    (stream, session)
}

/// A target that counts what reached it, so "refused" can be measured as *the
/// target was never dialled* instead of as an error string.
#[derive(Clone, Default)]
pub struct Target {
    pub connections: Arc<AtomicUsize>,
    pub bytes: Arc<AtomicUsize>,
}

/// Accept forever, count every connection and byte, and answer `reply` once per
/// connection.
///
/// It speaks nothing: a forwarder splices raw bytes and has no business knowing
/// what its target's protocol is, so a target that answers a fixed string is a
/// truthful stand-in for everything but an end-to-end handshake.
pub fn target_on(listener: TcpListener, reply: Vec<u8>) -> Target {
    let target = Target::default();
    let watch = target.clone();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            watch.connections.fetch_add(1, Ordering::SeqCst);
            let seen = Arc::clone(&watch.bytes);
            let reply = reply.clone();
            tokio::spawn(async move {
                let mut buf = vec![0_u8; 4096];
                let mut answered = false;
                loop {
                    match socket.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(read) => {
                            seen.fetch_add(read, Ordering::SeqCst);
                            if !answered {
                                answered = true;
                                if socket.write_all(&reply).await.is_err() {
                                    return;
                                }
                            }
                        }
                    }
                }
            });
        }
    });
    target
}

/// A Mac that receives a carried CONTROL frame over a duplex: its own node
/// key, and a pinned row for one peer already on its file, so the ordinary
/// CONTROL gate admits what that peer sends.
pub struct DuplexTarget {
    pub peers: PathBuf,
    pub state: PathBuf,
    pub key: NodeKey,
}

impl DuplexTarget {
    /// `dir` is the target's own scratch directory; `peer` is the one row it
    /// pins, labelled `the-far-side`, with every grant at its default.
    pub fn new(dir: &Path, peer: PeerId) -> Self {
        let peers = dir.join("tcr-peers.json");
        let state = dir.join("peer-state.json");
        let row = PeerRow {
            node: peer,
            label: "the-far-side".to_string(),
            endpoints: Vec::new(),
            added_at: 0,
            allow: peer_config::Allow::default(),
            lend: Vec::new(),
            rendezvous_secret: None,
            sees_us_at: None,
        };
        let file = PeerFile {
            peers: vec![row],
            ..PeerFile::default()
        };
        peer_config::save(&peers, &file).expect("write the peers file");
        let key = NodeKey::load_or_mint(dir).expect("mint a node key");
        Self { peers, state, key }
    }

    pub fn context(&self) -> SessionContext {
        SessionContext::new(&self.key, &self.peers, &self.state)
    }

    pub fn file(&self) -> PeerFile {
        peer_config::read_or_default(&self.peers).expect("the peers file reads")
    }
}

// ---------------------------------------------------------------------------
// The two tracing captures
// ---------------------------------------------------------------------------

/// One captured tracing event: its level, its message, and every field as a
/// string, the message included under `message`.
#[derive(Debug, Clone)]
pub struct CapturedEvent {
    pub level: tracing::Level,
    pub message: String,
    pub fields: BTreeMap<String, String>,
}

/// A layer that keeps every event it sees. Install it however the test needs
/// (globally in a binary of its own, or for one closure); the layer decides
/// nothing about that.
#[derive(Clone, Default)]
pub struct CaptureLayer {
    pub events: Arc<Mutex<Vec<CapturedEvent>>>,
}

struct FieldVisitor(BTreeMap<String, String>);

impl tracing::field::Visit for FieldVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().to_string(), format!("{value:?}"));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().to_string(), value.to_string());
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.0.insert(field.name().to_string(), value.to_string());
    }

    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CaptureLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut visitor = FieldVisitor(BTreeMap::new());
        event.record(&mut visitor);
        let message = visitor
            .0
            .get("message")
            .cloned()
            .unwrap_or_else(|| "<no message field>".to_string());
        self.events
            .lock()
            .expect("capture lock")
            .push(CapturedEvent {
                level: *event.metadata().level(),
                message,
                fields: visitor.0,
            });
    }
}

impl CaptureLayer {
    /// Every event whose message contains `needle`.
    pub fn matching(&self, needle: &str) -> Vec<CapturedEvent> {
        self.events
            .lock()
            .expect("capture lock")
            .iter()
            .filter(|event| event.message.contains(needle))
            .cloned()
            .collect()
    }

    /// Every event whose message is exactly `line`.
    pub fn matching_exactly(&self, line: &str) -> Vec<CapturedEvent> {
        self.events
            .lock()
            .expect("capture lock")
            .iter()
            .filter(|event| event.message == line)
            .cloned()
            .collect()
    }

    /// How many events this layer has seen, whatever they said.
    pub fn count(&self) -> usize {
        self.events.lock().expect("capture lock").len()
    }
}

/// One collected tracing event: the message, unquoted, and every OTHER field
/// as a string.
#[derive(Debug, Clone)]
pub struct LoggedEvent {
    pub message: String,
    pub fields: HashMap<String, String>,
}

/// A layer that collects every event, kept in order, for assertions about the
/// message and about which fields a line carries.
#[derive(Clone, Default)]
pub struct Collector {
    events: Arc<Mutex<Vec<LoggedEvent>>>,
}

impl Collector {
    pub fn events(&self) -> Vec<LoggedEvent> {
        self.events.lock().expect("the collector lock").clone()
    }

    pub fn matching(&self, needle: &str) -> Vec<LoggedEvent> {
        self.events()
            .into_iter()
            .filter(|event| event.message.contains(needle))
            .collect()
    }

    /// Where `needle` first appears in the event ORDER.
    ///
    /// Order is the assertion for a carry offer: a count alone cannot tell
    /// "offered after the fleet was walked" from "offered on the first
    /// account's connect failure", and the second one is the bug.
    pub fn first_index(&self, needle: &str) -> Option<usize> {
        self.events()
            .iter()
            .position(|event| event.message.contains(needle))
    }

    /// Every message, in order, for an assertion's failure output.
    pub fn messages(&self) -> Vec<String> {
        self.events()
            .into_iter()
            .map(|event| event.message)
            .collect()
    }
}

impl<S> tracing_subscriber::Layer<S> for Collector
where
    S: tracing::Subscriber,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        struct Visitor {
            message: String,
            fields: HashMap<String, String>,
        }
        impl tracing::field::Visit for Visitor {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                let rendered = format!("{value:?}");
                if field.name() == "message" {
                    self.message = rendered.trim_matches('"').to_string();
                } else {
                    self.fields.insert(field.name().to_string(), rendered);
                }
            }
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                if field.name() == "message" {
                    self.message = value.to_string();
                } else {
                    self.fields
                        .insert(field.name().to_string(), value.to_string());
                }
            }
        }
        let mut visitor = Visitor {
            message: String::new(),
            fields: HashMap::new(),
        };
        event.record(&mut visitor);
        self.events
            .lock()
            .expect("the collector lock")
            .push(LoggedEvent {
                message: visitor.message,
                fields: visitor.fields,
            });
    }
}
