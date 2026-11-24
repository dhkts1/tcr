//! Try to beat every abuse-resistance
//! cap.** Adversarial by construction, not descriptive.
//!
//! # What this file is for, and what it is not
//!
//! `tests/peer_pairing.rs` already asserts each cap's number once, in the
//! ordinary sequential shape: the fourth knock in ten seconds, the ninth
//! outstanding knock, the third unauthenticated socket, an `XX` from an
//! unaccepted instance. Every one of those is a test that the cap is WIRED.
//! None of them is an attempt to WIN, and a token bucket in particular cannot
//! be beaten by a sequential loop: by construction the loop hands the bucket
//! the one thing it needs, which is time between calls.
//!
//! So everything here is an attack with a shape the sequential tests cannot
//! produce: many callers released at once from a barrier, a fresh random
//! instance id per connection, a fresh proposed name per connection, a socket
//! that delivers a valid message 1 and then goes silent to hold what it
//! claimed, a burst timed to land exactly on the bucket's refill instant, and
//! a frame sized to the byte at the knock bound.
//!
//! # Everything runs on this box, against temp files
//!
//! Every listener binds `127.0.0.1:0` (the kernel picks the port), every peers
//! file and state file is a fresh temp file, and no test connects to a port it
//! did not bind itself. **Nothing here touches the proxy on `127.0.0.1:3456`**
//!: no test starts, stops, signals or dials it. Accounts, names and addresses
//! are obviously fake (`alice@example.com`, `192.0.2.0/24`, the documentation
//! range), because this repository is public.
//!
//! # One address is all a loopback test has
//!
//! macOS aliases only `127.0.0.1` onto `lo0` (measured: binding `127.0.0.2`
//! fails with `EADDRNOTAVAIL`), and adding an alias needs `ifconfig` as root.
//! So every cap whose grain is "per address" is attacked from the one address a
//! test can dial from, and the NODE-WIDE arm of the socket cap
//! ([`listener::MAX_UNAUTHENTICATED_SOCKETS`], 16 across all addresses) is not
//! reachable end to end here: the per-address arm refuses at
//! [`listener::MAX_UNAUTHENTICATED_PER_ADDRESS`] long before sixteen. That arm
//! is attacked at the counter the accept loop itself decides on
//! ([`listener::Admission`]), which is the same state and not a copy of it.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tcr::log_err::LogErr as _;

use tcr::peer::config::{self, Allow, Endpoint, EndpointSource, PeerFile, PeerRow};
use tcr::peer::discovery::{self, Discovered, FoundList, FoundSource};
use tcr::peer::id::NodeKey;
use tcr::peer::listener::{self, Admission, SessionContext};
use tcr::peer::noise;
use tcr::peer::pair;
use tcr::peer::state::{self, PeerState};
use tcr_peer_wire::{INSTANCE_ID_BYTES, InstanceId, Knock, PROTO_VERSION, PeerId};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

/// The caps at the knock door: the knock bucket, the pending-knock rows, the
/// unauthenticated sockets, the knock frame bound, the accepted window and the mute.
#[path = "peer_abuse/knock_caps.rs"]
mod knock_caps;

/// The caps on what a stranger can make this Mac keep: found rows, refusal log
/// lines, queued knocks and their reservations, and the window running out.
#[path = "peer_abuse/rows_and_queue.rs"]
mod rows_and_queue;

/// Which source addresses reach the knock path at all: the tailnet, and the
/// operator's own Macs on it.
#[path = "peer_abuse/tailnet_sources.rs"]
mod tailnet_sources;

/// The caps on a connection once it is open: accept errors, slow and headerless
/// sessions, the gateway allow-list, bans, hop budgets and a carry that died.
#[path = "peer_abuse/sessions_and_streams.rs"]
mod sessions_and_streams;

// ---------------------------------------------------------------------------
// The plan's numbers, transcribed: not the production constants
// ---------------------------------------------------------------------------

/// Every cap as `abuse-resistance.md` states it, written out as
/// a literal here.
///
/// # Why a second copy, when the production constant is `pub` and right there
///
/// Because an attack test that asserts against the constant it is defending
/// cannot fail when that constant is loosened. `assert_eq!(taken,
/// listener::KNOCK_BURST)` passes at a burst of 3 and passes just as happily at
/// a burst of 30: the mutation the positive control depends on is exactly the
/// one such an assertion is blind to, so the test would report success while
/// testing nothing.
///
/// So the assertions in this file are against these literals: the numbers the
/// record shipped, and [`the_caps_are_still_the_numbers_the_plan_shipped`]
/// pins each production constant to its literal in one place. Loosening any cap
/// therefore reds two tests: that one, and every attack that spends the cap.
///
/// This is the one duplication in the file, it is deliberate, and it is the
/// reason the positive controls in the report mean anything.
mod plan {
    use std::time::Duration;

    /// `1 knock / 10 s, burst 3, per address`.
    pub(super) const KNOCK_BURST: u32 = 3;
    /// `1 knock / 10 s, burst 3, per address`.
    pub(super) const KNOCK_INTERVAL_MS: i64 = 10_000;
    /// `at most 8 PENDING per node`.
    pub(super) const MAX_PENDING_KNOCKS: usize = 8;
    /// `16 concurrent unauthenticated sockets per node`.
    pub(super) const MAX_UNAUTHENTICATED_SOCKETS: usize = 16;
    /// `2 per address`.
    pub(super) const MAX_UNAUTHENTICATED_PER_ADDRESS: usize = 2;
    /// `5 s to deliver message 1`.
    pub(super) const MESSAGE_1_TIMEOUT: Duration = Duration::from_secs(5);
    /// The handshake bound the listener's own doc-comment states.
    pub(super) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
    /// `message 1 capped at 64 bytes` is the plan's figure for message 1
    /// itself; the KNOCK FRAME the session carries is bounded separately, and
    /// `listener::MAX_KNOCK_FRAME_BYTES` is the number the code shipped for it.
    pub(super) const MAX_KNOCK_FRAME_BYTES: usize = 512;
    /// `found list capped at 12 rows`.
    pub(super) const MAX_FOUND_ROWS: usize = 12;
    /// `at most 2 rows per source address`.
    pub(super) const MAX_FOUND_PER_ADDRESS: usize = 2;
    /// `a row that stops announcing leaves after 60 s`.
    pub(super) const FOUND_TTL_MS: i64 = 60_000;
    /// `an ignored address is muted for 1 h`.
    pub(super) const MUTE_MS: i64 = 3_600_000;
    /// `one log line per address per hour`.
    pub(super) const REFUSAL_LOG_QUIET_MS: i64 = 3_600_000;
    /// How many addresses that log remembers: the number the code shipped,
    /// and the cost of beating the line above.
    pub(super) const REFUSAL_LOG_ADDRESSES: usize = 1_024;
    /// `COMPARING has a 120 s window keyed to one instance id`.
    pub(super) const PAIRING_WINDOW_SECS: i64 = 120;
}

/// **Every cap is still the number the record shipped.**
///
/// The pin that makes every other test in this file a real control: the
/// assertions there are against [`plan`]'s literals, so this is the one place
/// that says those literals are what the code actually enforces. A cap
/// loosened in `src/peer/**` reds this test by name, with both numbers in the
/// message: which is what a positive control run needs to read.
///
/// These numbers close the three open questions `abuse-resistance.md` ends on
/// ("both", "both", ship the caps as written), so these numbers are settled and
/// this test is a ratchet, not a preference.
#[test]
fn the_caps_are_still_the_numbers_the_plan_shipped() {
    assert_eq!(
        listener::KNOCK_BURST,
        plan::KNOCK_BURST,
        "the knock burst shipped as {} and the code enforces {}",
        plan::KNOCK_BURST,
        listener::KNOCK_BURST
    );
    assert_eq!(
        listener::KNOCK_INTERVAL_MS,
        plan::KNOCK_INTERVAL_MS,
        "the knock interval shipped as {}ms and the code enforces {}ms",
        plan::KNOCK_INTERVAL_MS,
        listener::KNOCK_INTERVAL_MS
    );
    assert_eq!(
        state::MAX_PENDING_KNOCKS,
        plan::MAX_PENDING_KNOCKS,
        "the pending cap shipped as {} and the code enforces {}",
        plan::MAX_PENDING_KNOCKS,
        state::MAX_PENDING_KNOCKS
    );
    assert_eq!(
        listener::MAX_UNAUTHENTICATED_SOCKETS,
        plan::MAX_UNAUTHENTICATED_SOCKETS,
        "the node-wide socket cap shipped as {} and the code enforces {}",
        plan::MAX_UNAUTHENTICATED_SOCKETS,
        listener::MAX_UNAUTHENTICATED_SOCKETS
    );
    assert_eq!(
        listener::MAX_UNAUTHENTICATED_PER_ADDRESS,
        plan::MAX_UNAUTHENTICATED_PER_ADDRESS,
        "the per-address socket cap shipped as {} and the code enforces {}",
        plan::MAX_UNAUTHENTICATED_PER_ADDRESS,
        listener::MAX_UNAUTHENTICATED_PER_ADDRESS
    );
    assert_eq!(
        listener::MESSAGE_1_TIMEOUT,
        plan::MESSAGE_1_TIMEOUT,
        "the message-1 deadline shipped as {:?} and the code enforces {:?}",
        plan::MESSAGE_1_TIMEOUT,
        listener::MESSAGE_1_TIMEOUT
    );
    assert_eq!(
        listener::HANDSHAKE_TIMEOUT,
        plan::HANDSHAKE_TIMEOUT,
        "the handshake deadline shipped as {:?} and the code enforces {:?}",
        plan::HANDSHAKE_TIMEOUT,
        listener::HANDSHAKE_TIMEOUT
    );
    assert_eq!(
        listener::MAX_KNOCK_FRAME_BYTES,
        plan::MAX_KNOCK_FRAME_BYTES,
        "the knock frame bound shipped as {} bytes and the code enforces {}",
        plan::MAX_KNOCK_FRAME_BYTES,
        listener::MAX_KNOCK_FRAME_BYTES
    );
    assert_eq!(
        discovery::MAX_FOUND_ROWS,
        plan::MAX_FOUND_ROWS,
        "the found-row display cap shipped as {} and the code enforces {}",
        plan::MAX_FOUND_ROWS,
        discovery::MAX_FOUND_ROWS
    );
    assert_eq!(
        discovery::MAX_FOUND_PER_ADDRESS,
        plan::MAX_FOUND_PER_ADDRESS,
        "the per-address found cap shipped as {} and the code enforces {}",
        plan::MAX_FOUND_PER_ADDRESS,
        discovery::MAX_FOUND_PER_ADDRESS
    );
    assert_eq!(
        discovery::FOUND_TTL_MS,
        plan::FOUND_TTL_MS,
        "the found-row TTL shipped as {}ms and the code enforces {}ms",
        plan::FOUND_TTL_MS,
        discovery::FOUND_TTL_MS
    );
    assert_eq!(
        state::MUTE_MS,
        plan::MUTE_MS,
        "Ignore shipped as a {}ms mute and the code enforces {}ms",
        plan::MUTE_MS,
        state::MUTE_MS
    );
    assert_eq!(
        listener::REFUSAL_LOG_QUIET_MS,
        plan::REFUSAL_LOG_QUIET_MS,
        "the refusal log's quiet period shipped as {}ms and the code enforces {}ms",
        plan::REFUSAL_LOG_QUIET_MS,
        listener::REFUSAL_LOG_QUIET_MS
    );
    assert_eq!(
        listener::REFUSAL_LOG_ADDRESSES,
        plan::REFUSAL_LOG_ADDRESSES,
        "the refusal log's address bound shipped as {} and the code enforces {}",
        plan::REFUSAL_LOG_ADDRESSES,
        listener::REFUSAL_LOG_ADDRESSES
    );
    assert_eq!(
        pair::PAIRING_WINDOW_SECS,
        plan::PAIRING_WINDOW_SECS,
        "the Accept window shipped as {}s and the code enforces {}s",
        plan::PAIRING_WINDOW_SECS,
        pair::PAIRING_WINDOW_SECS
    );
    // And the listener's own spelling of that window is the same deadline, not
    // a second one: two spellings of one number is how the CLI and the listener
    // come to disagree about whether a window is open.
    assert_eq!(
        listener::ACCEPTED_WINDOW_SECS,
        pair::PAIRING_WINDOW_SECS,
        "the listener and the pairing module must name one deadline"
    );
}

// ---------------------------------------------------------------------------
// Scratch files and one Mac, the same shape `tests/peer_pairing.rs` uses
// ---------------------------------------------------------------------------

/// A scratch directory named after this process, thread and a caller tag, so
/// two tests in this binary (and several lanes running at once), never collide
/// on one path.
fn scratch(tag: &str) -> std::path::PathBuf {
    crate::peer_support::scratch_dir("abuse", tag)
}

/// One Mac's files: a peers file, a state file beside it, and a node key in the
/// same directory.
struct Node {
    peers: std::path::PathBuf,
    state: std::path::PathBuf,
    key: NodeKey,
}

impl Node {
    fn new(tag: &str) -> Self {
        let dir = scratch(tag);
        let peers = dir.join("tcr-peers.json");
        let state = dir.join("peer-state.json");
        config::save(&peers, &PeerFile::default()).expect("write the peers file");
        let key = NodeKey::load_or_mint(&dir).expect("mint a node key");
        Self { peers, state, key }
    }

    fn file(&self) -> PeerFile {
        config::read_or_default(&self.peers).expect("the peers file reads")
    }

    fn write_file(&self, file: &PeerFile) {
        config::save(&self.peers, file).expect("the peers file writes");
    }

    /// The state file as the LISTENER reads it: `load` runs `expire(now_ms)`
    /// on what it read, so a row's survival is a question about the clock the
    /// reader passes. Reading at zero: as `tests/peer_pairing.rs` does, where
    /// no test asks about an accepted window, expires every window an
    /// operator just opened, because `now` is then before the instant they
    /// opened it. Every read here passes the real clock for that reason.
    fn state(&self) -> PeerState {
        state::load(&self.state, pair::now_ms()).expect("the state file reads")
    }

    fn write_state(&self, value: &PeerState) {
        state::save(&self.state, value).expect("the state file writes");
    }

    fn context(&self) -> SessionContext {
        SessionContext::new(&self.key, &self.peers, &self.state)
    }
}

/// Bind `127.0.0.1:0`, start the SHIPPED accept loop on it, and report the
/// address the kernel chose.
///
/// `serve_on_with` and not a hand-rolled loop: every cap under attack here is
/// enforced inside it, so a harness that re-implemented the loop would be
/// attacking the harness.
async fn serve(context: SessionContext) -> SocketAddr {
    crate::peer_support::serve_shipped(context).await
}

/// The address every connection in this file comes from, in
/// [`state::knock_address`]'s form (the IP, with the ephemeral source port
/// dropped).
const LOOPBACK: &str = "127.0.0.1";

const fn instance(byte: u8) -> InstanceId {
    InstanceId([byte; INSTANCE_ID_BYTES])
}

/// A fresh random-looking instance id, so "instance-id churn" is really churn
/// and not a counter a coalescer could special-case. Derived from a hasher
/// rather than a CSPRNG: this is a label, and a test needs it unpredictable in
/// the sense of "not the same twice", nothing more.
fn churned_instance() -> InstanceId {
    use std::hash::{BuildHasher as _, Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    std::time::Instant::now().hash(&mut hasher);
    std::thread::current().id().hash(&mut hasher);
    let bits = hasher.finish().to_be_bytes();
    let mut id = [0_u8; INSTANCE_ID_BYTES];
    id.copy_from_slice(&bits[..INSTANCE_ID_BYTES]);
    // All-zero is the reservation placeholder's own id
    // (`state::Knock::is_reservation_placeholder`); a churned id that collided
    // with it would make a real row look like a placeholder.
    if id == [0_u8; INSTANCE_ID_BYTES] {
        id[0] = 1;
    }
    InstanceId(id)
}

/// Send one knock over a fresh connection and report whether the far side
/// answered with the ack byte.
async fn knock_at(addr: SocketAddr, id: InstanceId, name: Option<&str>) -> anyhow::Result<()> {
    let mut stream = TcpStream::connect(addr).await?;
    noise::send_knock(
        &mut stream,
        &Knock {
            instance_id: id,
            proposed_name: name.map(str::to_string),
            wire_version: PROTO_VERSION,
            listen_port: None,
        },
        None,
    )
    .await
}

/// Send one knock built by the caller, so a test can size the knock FRAME to
/// the byte.
async fn knock_frame_at(addr: SocketAddr, knock: &Knock) -> anyhow::Result<()> {
    let mut stream = TcpStream::connect(addr).await?;
    noise::send_knock(&mut stream, knock, None).await
}

/// Approve one pending knock the way `tcr peer accept` does, and report the
/// window it opened.
fn accept_pending(node: &Node, selector: &str) -> state::AcceptedInstance {
    let mut value = node.state();
    let window = value
        .accept_knock(selector, pair::now_ms(), pair::PAIRING_WINDOW_SECS)
        .expect("a pending row to accept");
    node.write_state(&value);
    window
}

/// A socket that counts what was WRITTEN BACK to it, so "zero bytes in answer"
/// is asserted on the wire and not on an error type.
///
/// A listener that answered first and refused afterwards satisfies every
/// assertion about its `Err` and fails this one.
async fn count_written(addr: SocketAddr, message_1: &[u8]) -> usize {
    let Ok(mut stream) = TcpStream::connect(addr).await else {
        // A refused connect wrote nothing either, which is the number this
        // function reports on.
        return 0;
    };
    let mut framed = Vec::with_capacity(2 + message_1.len());
    framed.extend_from_slice(
        &u16::try_from(message_1.len())
            .expect("a short frame")
            .to_be_bytes(),
    );
    framed.extend_from_slice(message_1);
    if stream.write_all(&framed).await.is_err() {
        return 0;
    }
    stream.flush().await.log_debug("flushing a test stream");

    let mut back = Vec::new();
    let read = tokio::time::timeout(
        plan::HANDSHAKE_TIMEOUT + Duration::from_secs(2),
        stream.read_to_end(&mut back),
    )
    .await;
    match read {
        Ok(Ok(_)) => back.len(),
        // A reset or a timeout is not an answer either, and the number that
        // matters is how many bytes came back before it.
        _ => back.len(),
    }
}

/// The bytes of a knock's `NN` message 1, built with a throwaway ephemeral
/// secret exactly as `noise::send_knock` builds its own. No payload: a knock's
/// payload rides the transport session message 2 establishes.
fn knock_message_1() -> Vec<u8> {
    let throwaway = tcr::peer::id::random_secret().expect("a throwaway secret");
    let mut state = noise::initiator_with_secret(&throwaway, noise::PATTERN_KNOCK, None, None)
        .expect("a knock initiator");
    let mut scratch = vec![0_u8; 1024];
    let len = state
        .write_message(&[], &mut scratch)
        .expect("knock message 1");
    scratch[..len].to_vec()
}

/// The bytes of an `XX` message 1 carrying `id` in the versioned pairing
/// payload, built with a throwaway static key: an impostor's key, which is the
/// point: nothing about the key is what the accepted window checks.
fn xx_message_1(id: InstanceId) -> Vec<u8> {
    let (secret, _) = noise::generate_static().expect("a keypair");
    let mut state = noise::initiator_with_secret(&secret, noise::PATTERN_PAIR, None, None)
        .expect("an XX initiator");
    let mut scratch = vec![0_u8; 1024];
    let len = state
        .write_message(&noise::pair_message_1_payload(&id), &mut scratch)
        .expect("XX message 1");
    scratch[..len].to_vec()
}

/// Open a connection, deliver `message_1`, and then hold the socket open
/// saying nothing: the "hold a slot without authenticating" attack. The
/// returned stream must be kept alive by the caller; dropping it releases
/// everything under attack.
async fn stall_after(addr: SocketAddr, message_1: &[u8]) -> TcpStream {
    let mut stream = TcpStream::connect(addr)
        .await
        .expect("connect to the test listener");
    let mut framed = Vec::with_capacity(2 + message_1.len());
    framed.extend_from_slice(
        &u16::try_from(message_1.len())
            .expect("a short frame")
            .to_be_bytes(),
    );
    framed.extend_from_slice(message_1);
    stream.write_all(&framed).await.expect("write message 1");
    stream.flush().await.expect("flush message 1");
    stream
}

/// Read one number off the counters the accept loop itself decides on.
fn with_admission<T>(context: &SessionContext, read: impl FnOnce(&mut Admission) -> T) -> T {
    let mut guard = match context.admission().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    read(&mut guard)
}

/// [`with_admission`] for a bare `Arc<Mutex<Admission>>` rather than one inside
/// a [`SessionContext`].
fn with_admission_arc<T>(
    admission: &Arc<std::sync::Mutex<Admission>>,
    read: impl FnOnce(&mut Admission) -> T,
) -> T {
    let mut guard = match admission.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    read(&mut guard)
}
