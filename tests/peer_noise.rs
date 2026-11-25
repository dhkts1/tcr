//! Phase 2's gates: the pin check, and what a bare pin is told.
//!
//! # Why the first one is the most important test in the design
//!
//! `snow` has no pin store. An `IK` responder learns the initiator's static key
//! after reading message 1, and **if the comparison against the pin store is
//! missing, the handshake completes and nothing errors**: every existing test
//! still passes, the connection works, and any node holding this node's public
//! key can open streams. There is no failure to observe unless someone writes
//! this test and watches it go red.
//!
//! So the property is asserted on the SOCKET, not on a log line and not on a
//! return value: after a refusal, the number of bytes the responder handed its
//! writer is zero. A responder that answered first and refused afterwards
//! passes every assertion about its error type and fails this one.
//!
//! # Local, and two listeners on one box
//!
//! Every test here runs entirely on this machine. They never read the
//! operator's config file, never touch the operator's peers file and
//! never signal a running proxy: each one binds `127.0.0.1:0` and lets the
//! kernel choose the port, and the two identities are keypairs generated in
//! this process with no files at all.
//!
//! # Nothing here is `#[ignore]`d
//!
//! Two tests were ignored because `PeerStore::open`, `PeerId::display` and
//! `sanitize_label` were `todo!()` elsewhere, so the bodies
//! panicked inside phase 1 instead of measuring phase 2. Those bodies are real
//! now and both tests run. An `#[ignore]` that outlives its reason is a gate
//! that has stopped measuring, so it goes at the same time as the blocker.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use tcr::log_err::LogErr as _;

use tcr::peer::config::join_key;
use tcr::peer::config::{Allow, ControlGrants, PeerRow};
use tcr::peer::listener::{
    self, RequestDedup, StreamRefusal, peer_stream_gate_hop, peer_stream_gate_rows,
};
use tcr::peer::noise::{self, Handshake, PinRefusal};
use tcr::peer::pair::{self, PairingWindow};
use tcr::peer::serve::{NodeFacts, hello_for_peer};
use tcr_peer_wire::{PeerId, StreamHeader, StreamKind, TunnelTarget};
use tokio::io::{AsyncWrite, ReadHalf, WriteHalf};
use tokio::net::{TcpListener, TcpStream};

/// Enrolment and the files it writes: the pairing window, the join token, the
/// refusal log, the peers and state files and their locks.
#[path = "peer_noise/enrolment_and_files.rs"]
mod enrolment_and_files;

// ---------------------------------------------------------------------------
// The instrument: a writer that counts, so "zero bytes written" is measured on
// the socket rather than inferred from an error type.
// ---------------------------------------------------------------------------

struct CountingWriter<W> {
    inner: W,
    written: Arc<AtomicUsize>,
}

impl<W: AsyncWrite + Unpin> AsyncWrite for CountingWriter<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let counted = Arc::clone(&self.written);
        let polled = Pin::new(&mut self.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = &polled {
            counted.fetch_add(*n, Ordering::SeqCst);
        }
        polled
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

type Counted = tokio::io::Join<ReadHalf<TcpStream>, CountingWriter<WriteHalf<TcpStream>>>;

/// Wrap a stream so every byte it writes is counted.
fn counted(stream: TcpStream) -> (Counted, Arc<AtomicUsize>) {
    let written = Arc::new(AtomicUsize::new(0));
    let (read, write) = tokio::io::split(stream);
    let writer = CountingWriter {
        inner: write,
        written: Arc::clone(&written),
    };
    (tokio::io::join(read, writer), written)
}

/// A pair of connected sockets on a kernel-assigned loopback port. Two
/// listeners in one test binary, which is what the design's two-machine test
/// looks like on one box.
async fn loopback_pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port");
    let addr = listener.local_addr().expect("the bound address");
    let dialing = tokio::spawn(async move { TcpStream::connect(addr).await });
    let (accepted, _) = listener.accept().await.expect("the connection");
    let dialed = dialing.await.expect("the dial task").expect("the dial");
    (accepted, dialed)
}

/// One pinned row for a static key, with every grant at its default.
fn pinned(key: [u8; 32]) -> PeerRow {
    PeerRow {
        node: PeerId(key),
        label: "studio-mac".to_string(),
        endpoints: Vec::new(),
        added_at: 0,
        rendezvous_secret: None,
        sees_us_at: None,
        allow: Allow::default(),
        lend: Vec::new(),
    }
}

/// A header for one stream kind, with a hop budget and no `via`.
const fn header(kind: StreamKind, target: Option<TunnelTarget>) -> StreamHeader {
    StreamHeader {
        kind,
        target,
        via: Vec::new(),
        hops_remaining: 1,
        request_id: 7,
    }
}

/// Dial a first pairing the way production does: `XX` with the versioned
/// message-1 payload (`noise::pair_message_1_payload`).
///
/// A helper rather than the bare `dial_handshake` these tests used to call,
/// because an `XX` message 1 with an EMPTY payload is 32 bytes: exactly an
/// `NN` knock, and the listener dispatches on that length. A test that dialled
/// without the payload would be testing the knock path while claiming to test
/// pairing, which is how a gate goes green about the wrong thing.
async fn dial_first_pairing<S>(
    stream: &mut S,
    secret: &[u8; 32],
) -> anyhow::Result<noise::PeerSession>
where
    S: tokio::io::AsyncRead + AsyncWrite + Unpin,
{
    noise::dial_handshake_with_payload(
        stream,
        secret,
        Handshake::Pair,
        None,
        None,
        &noise::pair_message_1_payload(&TEST_INSTANCE),
    )
    .await
}

/// The instance id every first-pairing dial in this file uses.
const TEST_INSTANCE: tcr_peer_wire::InstanceId =
    tcr_peer_wire::InstanceId([0x7E; tcr_peer_wire::INSTANCE_ID_BYTES]);

// ---------------------------------------------------------------------------
// The lengths the listener's "this is not a Noise message 1" check rests on
// ---------------------------------------------------------------------------

/// Every message-1 length is MEASURED against `snow`, not copied from the
/// spec, because the listener decides WHICH PATTERN ARRIVED from the length
/// alone (`Handshake::from_message_1_len`): a wrong constant is either a
/// pattern that can never connect or a length check that lets something else
/// through.
///
/// Four patterns now, and **the four lengths must stay pairwise distinct** :
/// that is asserted here rather than left implicit, because a collision is not
/// a compile error and the consequence is a knock dispatched as a first
/// pairing. It is also the reason an `XX` message 1 carries the 8-byte instance
/// id as its payload: with an empty payload it is 32 bytes, exactly an `NN`
/// message 1, and the two could not be told apart.
#[test]
fn noise_message_one_lengths_are_what_the_gate_pins() {
    let (secret, _) = noise::generate_static().expect("a static keypair");
    let (_, responder_public) = noise::generate_static().expect("a second static keypair");
    let mut scratch = [0_u8; 1024];

    let instance = tcr_peer_wire::InstanceId([0x11; tcr_peer_wire::INSTANCE_ID_BYTES]);
    let mut pairing = noise::initiator_with_secret(&secret, noise::PATTERN_PAIR, None, None)
        .expect("an XX initiator");
    let xx = pairing
        .write_message(&noise::pair_message_1_payload(&instance), &mut scratch)
        .expect("XX message 1");
    assert_eq!(
        xx,
        noise::XX_MESSAGE_1_LEN,
        "XX message 1 is one ephemeral plus the version byte and instance id it carries as \
         its payload; the listener refuses anything that is neither this nor the version-1 \
         length it answers with a refusal"
    );
    assert_eq!(
        noise::XX_MESSAGE_1_LEN_V1 + 1,
        noise::XX_MESSAGE_1_LEN,
        "the version byte is the whole difference between the two pairing lengths"
    );

    let mut returning = noise::initiator_with_secret(
        &secret,
        noise::PATTERN_RETURN,
        Some(&responder_public),
        None,
    )
    .expect("an IK initiator");
    let ik = returning
        .write_message(&[], &mut scratch)
        .expect("IK message 1");
    assert_eq!(
        ik,
        noise::IK_MESSAGE_1_LEN,
        "IK message 1 is an ephemeral, the initiator's encrypted static and two tags"
    );

    let (throwaway, _) = noise::generate_static().expect("an ephemeral-only keypair");
    let mut knocking = noise::initiator_with_secret(&throwaway, noise::PATTERN_KNOCK, None, None)
        .expect("an NN initiator");
    let nn = knocking
        .write_message(&[], &mut scratch)
        .expect("NN message 1");
    assert_eq!(
        nn,
        noise::KNOCK_MESSAGE_1_LEN,
        "NN message 1 is one ephemeral and nothing else: the knock itself is a transport \
         frame after the handshake"
    );

    let network_key = [0x44_u8; 32];
    let mut tagged = noise::initiator_with_secret(
        &throwaway,
        noise::PATTERN_KNOCK_PSK,
        None,
        Some(&network_key),
    )
    .expect("an NNpsk0 initiator");
    let nnpsk = tagged
        .write_message(&[], &mut scratch)
        .expect("NNpsk0 message 1");
    assert_eq!(
        nnpsk,
        noise::KNOCK_PSK_MESSAGE_1_LEN,
        "NNpsk0 mixes the psk before the first token, so message 1 already carries the \
         empty payload's AEAD tag"
    );

    // Pairwise distinct, and the dispatcher agrees with each. Without this, a
    // future pattern whose message 1 happened to be 40 bytes would be
    // dispatched as a first pairing and nothing would say so.
    let mut lengths = vec![nn, xx, nnpsk, ik];
    let before = lengths.len();
    lengths.sort_unstable();
    lengths.dedup();
    assert_eq!(
        lengths.len(),
        before,
        "two patterns share a message-1 length, so the listener cannot tell them apart: \
         NN={nn} XX={xx} NNpsk0={nnpsk} IK={ik}"
    );
    for (len, want) in [
        (nn, Handshake::Knock),
        (xx, Handshake::Pair),
        (nnpsk, Handshake::KnockPsk),
        (ik, Handshake::Return),
    ] {
        assert_eq!(
            Handshake::from_message_1_len(len),
            Some(want),
            "a {len}-byte first frame must dispatch to {want:?}"
        );
    }
    // And a length no pattern has dispatches to nothing, rather than to the
    // nearest match.
    assert_eq!(Handshake::from_message_1_len(33), None);
    assert_eq!(Handshake::from_message_1_len(0), None);

    // Every handshake message any pattern writes fits the scratch buffer the
    // drivers allocate. Measured against `snow` rather than reasoned about: the
    // buffer was shrunk from 65 kB to 256 bytes when the pre-authentication
    // socket slot stopped covering the handshake, and a buffer too small is a
    // handshake that fails at runtime with nothing to point at.
    let (a_secret, _a_public) = noise::generate_static().expect("a keypair");
    let (b_secret, b_public) = noise::generate_static().expect("a second keypair");
    let mut small = [0_u8; noise::HANDSHAKE_SCRATCH_BYTES];
    for (pattern, remote, psk) in [
        (noise::PATTERN_PAIR, None, None),
        (noise::PATTERN_RETURN, Some(&b_public), None),
        (noise::PATTERN_ENROL, Some(&b_public), Some(&network_key)),
        (noise::PATTERN_KNOCK, None, None),
        (noise::PATTERN_KNOCK_PSK, None, Some(&network_key)),
    ] {
        let mut initiator = noise::initiator_with_secret(&a_secret, pattern, remote, psk)
            .unwrap_or_else(|err| panic!("an initiator for {pattern}: {err:#}"));
        let mut responder = noise::responder_with_secret(
            &b_secret,
            pattern,
            match psk {
                Some(key) => std::slice::from_ref(key),
                None => &[],
            },
        )
        .unwrap_or_else(|err| panic!("a responder for {pattern}: {err:#}"));
        // Drive the whole pattern through buffers of exactly the size the
        // drivers use. `snow` errors rather than panicking on a short buffer,
        // so a too-small constant surfaces here as a refusal naming the
        // pattern.
        let mut out = [0_u8; noise::HANDSHAKE_SCRATCH_BYTES];
        let mut turn_initiator = true;
        while !initiator.is_handshake_finished() || !responder.is_handshake_finished() {
            let (writer, reader) = if turn_initiator {
                (&mut initiator, &mut responder)
            } else {
                (&mut responder, &mut initiator)
            };
            let payload: &[u8] = if turn_initiator && pattern == noise::PATTERN_PAIR {
                instance.as_bytes()
            } else {
                &[]
            };
            let len = writer
                .write_message(payload, &mut out)
                .unwrap_or_else(|err| panic!("{pattern} write into {} bytes: {err:#}", out.len()));
            reader
                .read_message(&out[..len], &mut small)
                .unwrap_or_else(|err| panic!("{pattern} read into {} bytes: {err:#}", small.len()));
            turn_initiator = !turn_initiator;
        }
    }

    // The allocation bound is the largest of the four, not a number from a
    // document: a 64-byte cap (which `abuse-resistance.md` names) would refuse
    // every returning peer and every headless enrolment.
    assert_eq!(
        noise::MAX_MESSAGE_1_BYTES,
        lengths.into_iter().max().unwrap_or(0),
        "the pre-authentication allocation bound must be the largest message 1 any pattern \
         this node speaks has, or a real peer is refused before it can authenticate"
    );
}

// ---------------------------------------------------------------------------
// The check that carries everything
// ---------------------------------------------------------------------------

/// **The check that carries everything.**
///
/// A peer that is not in the pin store is refused while the responder is
/// reading message 1: before message 2 is written, so nothing about this node
/// is disclosed to a stranger, and `tcr peer forget` takes effect on the very
/// next handshake with no restart.
///
/// Watch it fail by deleting the comparison in
/// [`tcr::peer::noise::pin_check`] and watching a forgotten peer
/// complete a handshake and open a stream.
#[test]
fn an_unpinned_peer_is_refused_before_message_two() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let peers = dir.path().join("tcr-peers.json");
    let store = tcr::peer::config::PeerStore::open(&peers).expect("an empty peers file opens");

    // A static key nobody pinned. Obviously fake, and 32 bytes, so the refusal
    // has to be the pin decision and not a length check.
    let stranger = [9_u8; 32];

    match noise::pin_check(&stranger, &store) {
        Err(PinRefusal::NotPinned { offered }) => {
            assert!(
                offered.starts_with("tcr-"),
                "the refusal must name the key that called, in display form, so the \
                 operator can tell which machine to go look at"
            );
        }
        Err(other) => panic!("wrong refusal: {other}"),
        Ok(peer) => panic!(
            "an unpinned key was admitted as {peer:?}: this is the failure mode the \
             whole design rests on, and it errors nowhere"
        ),
    }
}

/// A refused pin costs the caller ZERO bytes of answer.
///
/// This is the socket-level form of the test above, and it is the one that can
/// run on this branch: the authorization callback is injected, so the refusal
/// is a real `PinRefusal` built here rather than one that needs
/// `PeerId::display`. Watch it fail by moving the `authorize(...)` call in
/// `noise::finish_responder` to after `write_frame`, which is exactly the
/// mistake the design says nothing would catch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_pin_gets_zero_bytes_in_answer() {
    let (responder_secret, responder_public) = noise::generate_static().expect("a keypair");
    let (initiator_secret, _) = noise::generate_static().expect("a second keypair");
    let (server, mut client) = loopback_pair().await;
    let (mut server, written) = counted(server);

    let dialing = tokio::spawn(async move {
        noise::dial_handshake(
            &mut client,
            &initiator_secret,
            Handshake::Return,
            Some(&responder_public),
            None,
        )
        .await
    });

    let message_1 = noise::read_frame(&mut server)
        .await
        .expect("message 1 arrives");
    let mut scratch = vec![0_u8; 65535];
    let state = noise::read_message_1(
        &responder_secret,
        Handshake::Return,
        &[],
        &message_1,
        &mut scratch,
    )
    .expect("message 1 authenticates: the refusal under test is the PIN, not the crypto");

    let refused = noise::finish_responder(&mut server, state, Handshake::Return, |remote| {
        assert_eq!(
            remote.len(),
            32,
            "an IK responder learns a 32-byte static here"
        );
        Err(PinRefusal::NotPinned {
            offered: "tcr-notpinned".to_string(),
        })
    })
    .await;

    let error = refused.expect_err("a refused pin must not complete a handshake");
    assert!(
        error.downcast_ref::<PinRefusal>().is_some(),
        "the refusal must survive as a PinRefusal so a caller can log which key called: {error:#}"
    );
    assert_eq!(
        written.load(Ordering::SeqCst),
        0,
        "the responder wrote bytes after refusing the pin: a stranger learned this node's \
         static key, and `tcr peer forget` stopped meaning anything"
    );

    // Close this side so the initiator's read ends instead of waiting on an
    // answer that is never coming: the refusal closes the connection, which is
    // what the responder would do in production.
    drop(server);
    assert!(
        dialing.await.expect("the dial task").is_err(),
        "the initiator must not complete a handshake nobody answered"
    );
}

/// A remote static of the wrong width is a malformed handshake, not a peer, and
/// the refusal names the width it got.
#[test]
fn pin_check_refuses_a_static_of_the_wrong_width() {
    let rows = [pinned([4_u8; 32])];
    match noise::pin_check_rows(&[4_u8; 31], &rows) {
        Err(PinRefusal::Malformed { len }) => assert_eq!(len, 31),
        other => panic!("31 bytes is not a static key: {other:?}"),
    }
}

/// A pinned key is admitted as itself, and by identity rather than by position
/// in the file.
#[test]
fn pin_check_admits_a_pinned_key() {
    let rows = [pinned([1_u8; 32]), pinned([2_u8; 32])];
    let admitted = noise::pin_check_rows(&[2_u8; 32], &rows).expect("a pinned key is admitted");
    assert_eq!(admitted, PeerId([2_u8; 32]));
}

// ---------------------------------------------------------------------------
// Enrolment: the PSK is proved inside message 1
// ---------------------------------------------------------------------------

/// A wrong join secret fails while the responder is reading message 1, and the
/// responder writes **zero bytes** in answer.
///
/// Asserted on the socket's write count, not on a log line: putting the PSK in
/// message 1 is the entire reason this property exists, and a log line would
/// pass just as happily if the responder answered first and refused after.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrong_join_secret_gets_no_answer_at_all() {
    let (registrar_secret, registrar_public) = noise::generate_static().expect("a keypair");
    let (joiner_secret, _) = noise::generate_static().expect("a second keypair");
    let outstanding = [[7_u8; 32]];
    let wrong = [8_u8; 32];

    let (server, mut client) = loopback_pair().await;
    let (mut server, written) = counted(server);

    let dialing = tokio::spawn(async move {
        noise::dial_handshake(
            &mut client,
            &joiner_secret,
            Handshake::Enrol,
            Some(&registrar_public),
            Some(&wrong),
        )
        .await
    });

    let refused = noise::accept_handshake(
        &mut server,
        &registrar_secret,
        Handshake::Enrol,
        &outstanding,
        |remote| match <[u8; 32]>::try_from(remote) {
            Ok(key) => Ok(PeerId(key)),
            Err(_) => Err(PinRefusal::Malformed { len: remote.len() }),
        },
    )
    .await;

    assert!(
        refused.is_err(),
        "a wrong secret must not enrol: the psk is mixed in before message 1 is read"
    );
    assert_eq!(
        written.load(Ordering::SeqCst),
        0,
        "the registrar answered a wrong join secret: psk1's whole purpose is that it \
         cannot, because a wrong secret fails before message 2 exists"
    );
    drop(server);
    assert!(dialing.await.expect("the dial task").is_err());
}

/// The right secret enrols, over a real socket, and both sides end up holding
/// the same peer id: the joiner's is the registrar's static from the token, and
/// the registrar's is the one message 1 proved.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_right_join_secret_enrols() {
    let (registrar_secret, registrar_public) = noise::generate_static().expect("a keypair");
    let (joiner_secret, joiner_public) = noise::generate_static().expect("a second keypair");
    let secret = [7_u8; 32];

    let (server, mut client) = loopback_pair().await;
    let dialing = tokio::spawn(async move {
        noise::dial_handshake(
            &mut client,
            &joiner_secret,
            Handshake::Enrol,
            Some(&registrar_public),
            Some(&secret),
        )
        .await
    });

    let mut server = server;
    let accepted = noise::accept_handshake(
        &mut server,
        &registrar_secret,
        Handshake::Enrol,
        &[secret],
        |remote| match <[u8; 32]>::try_from(remote) {
            Ok(key) => Ok(PeerId(key)),
            Err(_) => Err(PinRefusal::Malformed { len: remote.len() }),
        },
    )
    .await
    .expect("the right secret enrols even though nothing is pinned yet");

    let joined = dialing
        .await
        .expect("the dial task")
        .expect("the joiner completes");
    assert_eq!(
        accepted.peer,
        PeerId(joiner_public),
        "the registrar pins the key message 1 proved, not one a beacon claimed"
    );
    assert_eq!(joined.peer, PeerId(registrar_public));
    assert_eq!(
        accepted.code, joined.code,
        "both ends derive the same handshake hash, which is what makes the six digits a \
         channel binding"
    );
}

// ---------------------------------------------------------------------------
// The listener speaks Noise and nothing else
// ---------------------------------------------------------------------------

/// First bytes that are not a Noise message 1 close the socket with nothing
/// written: no banner, no version, no error frame.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn first_bytes_that_are_not_a_noise_message_one_get_nothing() {
    let (secret, _) = noise::generate_static().expect("a keypair");
    let (server, mut client) = loopback_pair().await;
    let (mut server, written) = counted(server);

    let probing = tokio::spawn(async move {
        // A plausible scan: an HTTP request line, framed so the read succeeds
        // and the DECISION is about the content rather than the length prefix.
        noise::write_frame(&mut client, b"GET / HTTP/1.1\r\n\r\n").await
    });

    // `Handshake::Return` here is arbitrary: the point is that the frame's
    // LENGTH does not match any real message 1, so `accept_handshake` refuses
    // it before `snow` sees a byte, whichever pattern it is checked against.
    let refused = noise::accept_handshake(&mut server, &secret, Handshake::Return, &[], |remote| {
        noise::pin_check_rows(remote, &[])
    })
    .await;
    assert!(refused.is_err(), "an HTTP request is not a Noise message 1");
    assert_eq!(
        written.load(Ordering::SeqCst),
        0,
        "the peer socket answered a scanner; it must close with nothing written"
    );
    probing
        .await
        .expect("the probe task")
        .expect("the probe wrote its bytes");
}

/// An `IK` dial to a responder holding a different static key gets no answer at
/// all: the responder cannot decrypt message 1, so it never reaches message 2.
/// This is the wrong-key case from the initiator's side, and it costs the
/// wrong responder zero written bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ik_dial_to_the_wrong_responder_key_gets_no_answer() {
    let (responder_secret, _) = noise::generate_static().expect("a keypair");
    let (_, other_public) = noise::generate_static().expect("a second keypair");
    let (initiator_secret, initiator_public) = noise::generate_static().expect("a third keypair");

    let (server, mut client) = loopback_pair().await;
    let (mut server, written) = counted(server);

    let dialing = tokio::spawn(async move {
        noise::dial_handshake(
            &mut client,
            &initiator_secret,
            Handshake::Return,
            // The pinned key of some OTHER machine.
            Some(&other_public),
            None,
        )
        .await
    });

    let rows = [pinned(initiator_public)];
    let refused = noise::accept_handshake(
        &mut server,
        &responder_secret,
        Handshake::Return,
        &[],
        |remote| noise::pin_check_rows(remote, &rows),
    )
    .await;
    assert!(
        refused.is_err(),
        "message 1 encrypted to another node's static key cannot be read here"
    );
    assert_eq!(
        written.load(Ordering::SeqCst),
        0,
        "a wrong-key dial drew an answer out of this node"
    );
    drop(server);
    assert!(dialing.await.expect("the dial task").is_err());
}

// ---------------------------------------------------------------------------
// The six digits, and the return visit
// ---------------------------------------------------------------------------

/// The interactive path's six digits come from the handshake hash, which binds
/// both statics AND both ephemerals: so a relayed handshake produces two
/// different codes and the operators see it. A hash of two pasted fingerprints
/// would not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_six_digit_code_binds_the_handshake_not_the_keys() {
    let (server_secret, server_public) = noise::generate_static().expect("a keypair");
    let (client_secret, client_public) = noise::generate_static().expect("a second keypair");

    let mut codes = Vec::new();
    for _ in 0..2 {
        let (mut server, mut client) = loopback_pair().await;
        let secret = client_secret;
        let dialing = tokio::spawn(async move { dial_first_pairing(&mut client, &secret).await });
        // `Handshake::Pair` never invokes `authorize`: see
        // `noise::accept_handshake`'s doc comment: so this closure is never
        // called; it exists only because the signature needs one.
        let accepted =
            noise::accept_handshake(&mut server, &server_secret, Handshake::Pair, &[], |_| {
                unreachable!("Handshake::Pair does not call authorize before message 2")
            })
            .await
            .expect("a first pairing needs nothing pinned");
        let dialed = dialing
            .await
            .expect("the dial task")
            .expect("the pairing completes");

        assert_eq!(
            accepted.code, dialed.code,
            "both screens must show the same six digits or the compare is theatre"
        );
        assert_eq!(
            accepted.code.len(),
            6,
            "six digits, fixed width, on both screens"
        );
        assert!(accepted.code.chars().all(|c| c.is_ascii_digit()));
        assert_eq!(accepted.peer, PeerId(client_public));
        assert_eq!(dialed.peer, PeerId(server_public));
        codes.push(accepted.code);
    }

    assert_ne!(
        codes[0], codes[1],
        "two XX handshakes between the SAME two static keys produced the same code, so the \
         code is a function of the keys alone and a man in the middle who relays both \
         halves would show the operators one matching number"
    );
}

// ---------------------------------------------------------------------------
// The six digits commit to a nonce from each side
// ---------------------------------------------------------------------------

/// Answer an `XX` dial as a responder that reveals a DIFFERENT nonce than the
/// one it committed to in message 2.
///
/// This is the man in the middle, reduced to the one move the commitment
/// forbids: pick the nonce after seeing the other side's, so the six digits
/// come out wherever you want them. `reveal` is what it sends in place of the
/// nonce it hashed.
async fn answer_pairing_revealing<S>(
    stream: &mut S,
    secret: &[u8; 32],
    committed: &[u8; noise::PAIR_NONCE_BYTES],
    reveal: &[u8; noise::PAIR_NONCE_BYTES],
) -> anyhow::Result<()>
where
    S: tokio::io::AsyncRead + AsyncWrite + Unpin,
{
    let mut state = noise::responder_with_secret(secret, noise::PATTERN_PAIR, &[])?;
    let mut scratch = vec![0_u8; noise::HANDSHAKE_SCRATCH_BYTES];

    let message_1 = noise::read_frame(stream).await?;
    state.read_message(&message_1, &mut scratch)?;

    let len = state.write_message(&noise::nonce_commitment(committed), &mut scratch)?;
    noise::write_frame(stream, &scratch[..len]).await?;

    let message_3 = noise::read_frame(stream).await?;
    state.read_message(&message_3, &mut scratch)?;

    let mut transport = noise::into_transport(state)?;
    noise::send_encrypted(stream, &mut transport, reveal).await
}

/// **A responder that reveals a nonce it did not commit to is refused, and
/// nothing is pinned.**
///
/// The attack the commitment closes: a machine in the middle answers the
/// dialling Mac, learns the digits it will show, and then picks its own
/// contribution so the OTHER handshake it is running comes out with the same
/// number. Choosing after the fact is exactly what this test does, and the
/// dialling side refuses before it has a code to show anybody.
///
/// Watch it fail by removing the `nonce_commitment(&revealed) != commitment`
/// check in `dial_handshake_with_payload`: the dial then completes and hands
/// back six digits derived from a nonce the responder chose last.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pairing_nonce_that_does_not_match_its_commitment_is_refused() {
    let (server_secret, _) = noise::generate_static().expect("a keypair");
    let (client_secret, _) = noise::generate_static().expect("a second keypair");
    let (mut server, mut client) = loopback_pair().await;

    let committed = [0x11_u8; noise::PAIR_NONCE_BYTES];
    let reveal = [0x22_u8; noise::PAIR_NONCE_BYTES];
    let answering = tokio::spawn(async move {
        answer_pairing_revealing(&mut server, &server_secret, &committed, &reveal).await
    });

    let refused = dial_first_pairing(&mut client, &client_secret)
        .await
        .expect_err("a reveal that misses its commitment is not a pairing");
    let text = format!("{refused:#}");
    assert!(
        text.contains("committed to"),
        "the refusal names what went wrong, so an operator is not left guessing: {text}"
    );
    answering
        .await
        .expect("the answering task")
        .log_debug("answering a pairing whose reveal misses");
}

/// **The digits move with the responder's nonce, on one handshake hash.**
///
/// The grind the old derivation allowed was: fix the digits by choosing keys,
/// because the code was a function of the handshake hash alone. Here the hash
/// is held CONSTANT and only the responder's nonce moves, and the code changes:
/// so an attacker who has ground its static key to some hash still cannot say
/// what the digits will be, because half the input arrives after it is
/// committed.
///
/// Watch it fail by dropping the nonces from `pairing_code`'s digest.
#[test]
fn the_pairing_code_changes_with_the_responder_nonce() {
    let hash = [0x5a_u8; 32];
    let initiator = [0x01_u8; noise::PAIR_NONCE_BYTES];

    let one = noise::pairing_code(&hash, &initiator, &[0x02_u8; noise::PAIR_NONCE_BYTES]);
    let two = noise::pairing_code(&hash, &initiator, &[0x03_u8; noise::PAIR_NONCE_BYTES]);
    assert_ne!(
        one, two,
        "one handshake hash and two responder nonces must not give one code, or the nonce \
         contributes nothing"
    );

    // And with the initiator's, for the same reason in the other direction.
    let three = noise::pairing_code(
        &hash,
        &[0x04_u8; noise::PAIR_NONCE_BYTES],
        &[0x02_u8; noise::PAIR_NONCE_BYTES],
    );
    assert_ne!(one, three, "the initiator's nonce is an input too");
    assert_eq!(one.len(), 6, "six digits, fixed width");
    assert!(one.chars().all(|c| c.is_ascii_digit()));

    // The same three inputs give the same answer on both screens, which is the
    // only reason the compare means anything.
    assert_eq!(
        one,
        noise::pairing_code(&hash, &initiator, &[0x02_u8; noise::PAIR_NONCE_BYTES])
    );
}

/// **A Mac on the older build is told which version it speaks, and nothing is
/// written back.**
///
/// Version 1 carried the bare instance id and derived the digits from the
/// handshake hash alone. It is recognized by length so the operator reads
/// "update the other Mac" rather than "those bytes are not a Noise message 1",
/// and it is refused rather than served under the weaker rule.
#[test]
fn a_version_one_pairing_payload_is_refused_by_name() {
    let instance = tcr_peer_wire::InstanceId([0x7E; tcr_peer_wire::INSTANCE_ID_BYTES]);

    assert_eq!(
        Handshake::from_message_1_len(noise::XX_MESSAGE_1_LEN_V1),
        Some(Handshake::Pair),
        "the older length still dispatches as a first pairing, or the refusal below is \
         never reached"
    );

    let (secret, _) = noise::generate_static().expect("a keypair");
    let mut state = noise::initiator_with_secret(&secret, noise::PATTERN_PAIR, None, None)
        .expect("an XX initiator");
    let mut scratch = vec![0_u8; noise::HANDSHAKE_SCRATCH_BYTES];
    let len = state
        .write_message(instance.as_bytes(), &mut scratch)
        .expect("a version-1 XX message 1");
    assert_eq!(len, noise::XX_MESSAGE_1_LEN_V1);

    let (responder_secret, _) = noise::generate_static().expect("a second keypair");
    let mut responder_scratch = vec![0_u8; noise::HANDSHAKE_SCRATCH_BYTES];
    let read = noise::read_message_1_matching(
        &responder_secret,
        Handshake::Pair,
        &[],
        &scratch[..len],
        &mut responder_scratch,
    )
    .expect("a version-1 message 1 still reads");

    let refused = read
        .instance_id()
        .expect_err("a version-1 payload is not served");
    let text = format!("{refused:#}");
    assert!(
        text.contains("version 1"),
        "the refusal names the version the other Mac speaks: {text}"
    );
    assert!(
        text.contains("nothing was written"),
        "and says the connection cost that Mac no answer: {text}"
    );

    // The control: the version this node writes is read back as the instance
    // id it carries, so the refusal above is about the version and not about
    // every payload.
    let mut state = noise::initiator_with_secret(&secret, noise::PATTERN_PAIR, None, None)
        .expect("an XX initiator");
    let len = state
        .write_message(&noise::pair_message_1_payload(&instance), &mut scratch)
        .expect("a version-2 XX message 1");
    let read = noise::read_message_1_matching(
        &responder_secret,
        Handshake::Pair,
        &[],
        &scratch[..len],
        &mut responder_scratch,
    )
    .expect("message 1 reads");
    assert_eq!(
        read.instance_id().expect("the instance id parses"),
        instance
    );
}

/// A return visit to a pinned key completes in ONE round trip: the initiator
/// writes message 1, the responder writes message 2, and the session is open.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_return_visit_completes_in_one_round_trip() {
    let (responder_secret, responder_public) = noise::generate_static().expect("a keypair");
    let (initiator_secret, initiator_public) = noise::generate_static().expect("a second keypair");

    let (server, mut client) = loopback_pair().await;
    let (mut server, written) = counted(server);
    let dialing = tokio::spawn(async move {
        noise::dial_handshake(
            &mut client,
            &initiator_secret,
            Handshake::Return,
            Some(&responder_public),
            None,
        )
        .await
    });

    let rows = [pinned(initiator_public)];
    let accepted = noise::accept_handshake(
        &mut server,
        &responder_secret,
        Handshake::Return,
        &[],
        |remote| noise::pin_check_rows(remote, &rows),
    )
    .await
    .expect("a pinned peer returns");
    let dialed = dialing
        .await
        .expect("the dial task")
        .expect("the return completes");

    assert_eq!(accepted.peer, PeerId(initiator_public));
    assert_eq!(dialed.peer, PeerId(responder_public));
    // Message 2 is 48 bytes (an ephemeral plus the empty payload's tag) and the
    // frame adds a two-byte prefix. One frame, one round trip: anything more
    // and IK's whole reason for being chosen over KK is gone.
    assert_eq!(
        written.load(Ordering::SeqCst),
        50,
        "the responder wrote something other than exactly one 48-byte message 2"
    );
}

// ---------------------------------------------------------------------------
// What a bare pin is told, and what a peer-controlled header buys
// ---------------------------------------------------------------------------

/// A bare pin is told booleans and addresses, and **nothing countable**.
///
/// Without the three control grants, pinning a peer would silently subscribe it
/// to per-window lendable amounts with account counts, this build's sha, its
/// boot id, and a list of this node's other peers.
///
/// Watch it fail by attaching `lendable` unconditionally.
#[test]
fn hello_for_a_bare_pin_is_minimal() {
    let mut facts = NodeFacts::minimal(PeerId([3_u8; 32]));
    facts.label = "studio-mac".to_string();
    facts.addrs = vec!["192.0.2.10:7777".to_string()];
    facts.hops_to_egress = Some(1);
    facts.build_sha = "deadbeef".to_string();
    facts.boot_id = 42;

    let bare = hello_for_peer(&facts, &ControlGrants::default());
    assert_eq!(
        bare.lendable, None,
        "a bare pin is told no countable figure"
    );
    assert_eq!(bare.hops_to_egress, None);
    assert_eq!(
        bare.briefs, None,
        "a bare pin is told nothing about this node's other peers"
    );
    assert_eq!(bare.build_sha, None);
    assert_eq!(bare.boot_id, None);
    assert_eq!(
        bare.label, "studio-mac",
        "a label and addresses are what a pin does buy"
    );
    assert_eq!(bare.addrs, vec!["192.0.2.10:7777".to_string()]);

    // And the grants are what add each block, so the test cannot pass because
    // the fields are always None.
    let granted = hello_for_peer(
        &facts,
        &ControlGrants {
            briefs: true,
            lendable: true,
            diag: true,
            drop: true,
        },
    );
    assert_eq!(granted.hops_to_egress, Some(1));
    assert_eq!(granted.build_sha.as_deref(), Some("deadbeef"));
    assert_eq!(granted.boot_id, Some(42));
    assert!(granted.lendable.is_some());
    assert!(granted.briefs.is_some());
}

/// A kind this build does not know is REFUSED, never ignored and never degraded
/// to one it does know: a newer peer on the same LAN speaks a kind this build
/// has no handler for, and a handler that treated it as a tunnel would be an
/// open relay.
#[test]
fn an_unknown_stream_kind_is_refused_not_ignored() {
    let row = pinned([5_u8; 32]);
    let refusal = peer_stream_gate_rows(&header(StreamKind::from(9999), None), Some(&row))
        .expect_err("an unknown kind is refused");
    assert_eq!(refusal, StreamRefusal::UnknownKind(9999));
}

/// A bare pin may say hello and nothing else. Every grant defaults to false, so
/// this is what an unconfigured mesh answers to everything.
#[test]
fn a_bare_pin_is_granted_control_and_nothing_else() {
    let row = pinned([5_u8; 32]);
    assert_eq!(
        peer_stream_gate_rows(&header(StreamKind::Control, None), Some(&row)),
        Ok(())
    );
    assert_eq!(
        peer_stream_gate_rows(&header(StreamKind::Serve, None), Some(&row)),
        Err(StreamRefusal::NotGranted(StreamKind::Serve)),
        "SERVE means this node reads that peer's requests in full; it needs allow.inspect"
    );
    let origin = Some(TunnelTarget::Origin {
        host: "api.anthropic.com".to_string(),
        port: 443,
    });
    assert_eq!(
        peer_stream_gate_rows(&header(StreamKind::Tunnel, origin), Some(&row)),
        Err(StreamRefusal::NotGranted(StreamKind::Tunnel)),
        "carrying bytes out needs allow.gateway"
    );
    assert_eq!(
        peer_stream_gate_rows(
            &header(
                StreamKind::Tunnel,
                Some(TunnelTarget::Peer {
                    node: PeerId([6_u8; 32])
                })
            ),
            Some(&row)
        ),
        Err(StreamRefusal::NotGranted(StreamKind::Tunnel)),
        "forwarding to another pinned peer needs allow.relay"
    );
}

/// A forgotten peer is refused on the very next frame, with no restart: the
/// row is gone, and an absent row is the same answer as "not authorized for
/// anything".
#[test]
fn a_forgotten_peer_is_refused_on_the_next_frame() {
    assert_eq!(
        peer_stream_gate_rows(&header(StreamKind::Control, None), None),
        Err(StreamRefusal::NotGranted(StreamKind::Control)),
        "a peer whose row was deleted keeps its session open until the next frame"
    );
}

/// A spent hop budget, a loop and a duplicate are each their own refusal, and
/// the hop checks read this node's own id rather than trusting `via`.
#[test]
fn a_spent_budget_a_loop_and_a_duplicate_are_each_refused() {
    let row = pinned([5_u8; 32]);
    let mut spent = header(StreamKind::Control, None);
    spent.hops_remaining = 0;
    assert_eq!(
        peer_stream_gate_rows(&spent, Some(&row)),
        Err(StreamRefusal::HopsExhausted)
    );

    let me = PeerId([1_u8; 32]);
    let mut looping = header(StreamKind::Control, None);
    looping.via = vec![PeerId([9_u8; 32]), me];
    let mut dedup = RequestDedup::new();
    assert_eq!(
        peer_stream_gate_hop(&me, &looping, &mut dedup, 0),
        Err(StreamRefusal::LoopDetected),
        "a frame that has already been through this node is a cycle, killed outright"
    );

    let fresh = header(StreamKind::Control, None);
    assert_eq!(peer_stream_gate_hop(&me, &fresh, &mut dedup, 0), Ok(()));
    assert_eq!(
        peer_stream_gate_hop(&me, &fresh, &mut dedup, 0),
        Err(StreamRefusal::Duplicate),
        "a diamond delivery served twice is two debits for one request"
    );
    assert_eq!(
        peer_stream_gate_hop(&me, &fresh, &mut dedup, 700_000),
        Ok(()),
        "the dedup cache is TTL-bounded; a cache a peer can grow forever is a memory bug \
         with a security label"
    );
}

// ---------------------------------------------------------------------------
// The control loop polls once a second, so a frame may straddle a tick
// ---------------------------------------------------------------------------

/// A frame whose halves arrive 1.5 seconds apart still arrives, and the session
/// still decrypts the frame after it.
///
/// # The bug this is the control for
///
/// `serve_control` wraps its read in `timeout(HANDOFF_POLL_INTERVAL, ..)` so an
/// idle borrower still gets its bearer renewed. Built on `recv_encrypted`, that
/// read is `read_exact`, which is not cancel safe: the bytes it had already
/// taken off the socket die with the cancelled future at the tick. The next
/// read then starts in the MIDDLE of the frame and reads ciphertext as a length
/// prefix, so the Noise nonce desyncs and the session dies blaming the session
/// key. On a LAN no frame is ever slow enough to see it; one WAN retransmit, or
/// a laptop that slept between the prefix and the body, is enough.
///
/// The loop below is the production loop's shape on purpose, ticks and all.
/// The tick count is asserted rather than ignored, because a test where the
/// timeout never fired would pass against the torn reader too and measure
/// nothing.
///
/// Watch it fail: swap `frames.recv_encrypted(..)` for
/// `noise::recv_encrypted(&mut server, &mut accepted.transport)`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frame_split_across_the_poll_tick_survives() {
    use tokio::io::AsyncWriteExt;

    let (responder_secret, responder_public) = noise::generate_static().expect("a keypair");
    let (initiator_secret, initiator_public) = noise::generate_static().expect("a second keypair");

    let (mut server, mut client) = loopback_pair().await;
    let dialing = tokio::spawn(async move {
        let mut dialed = noise::dial_handshake(
            &mut client,
            &initiator_secret,
            Handshake::Return,
            Some(&responder_public),
            None,
        )
        .await
        .expect("the return completes");

        // Both frames encrypted up front, into memory, so the split below is a
        // decision about WHEN bytes reach the socket and nothing else.
        let mut wire: Vec<u8> = Vec::new();
        noise::send_encrypted(&mut wire, &mut dialed.transport, b"first")
            .await
            .expect("the first frame encrypts");
        let first_frame_len = wire.len();
        noise::send_encrypted(&mut wire, &mut dialed.transport, b"second")
            .await
            .expect("the second frame encrypts");

        // Three bytes: the whole length prefix and one byte of ciphertext, so
        // the reader is provably inside the frame when the tick lands.
        client.write_all(&wire[..3]).await.expect("the prefix goes");
        client.flush().await.expect("the prefix flushes");
        tokio::time::sleep(std::time::Duration::from_millis(1_500)).await;
        client
            .write_all(&wire[3..first_frame_len])
            .await
            .expect("the rest of the first frame goes");
        client
            .write_all(&wire[first_frame_len..])
            .await
            .expect("the second frame goes");
        client.flush().await.expect("the body flushes");
        // Hold the socket open: a close would be a second way for the reader to
        // finish and this test is about the first one.
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    });

    let rows = [pinned(initiator_public)];
    let mut accepted = noise::accept_handshake(
        &mut server,
        &responder_secret,
        Handshake::Return,
        &[],
        |remote| noise::pin_check_rows(remote, &rows),
    )
    .await
    .expect("a pinned peer returns");

    let mut frames = noise::FrameReader::new();
    let mut read: Vec<Vec<u8>> = Vec::new();
    let mut ticks = 0_u32;
    while read.len() < 2 {
        match tokio::time::timeout(
            std::time::Duration::from_secs(1),
            frames.recv_encrypted(&mut server, &mut accepted.transport),
        )
        .await
        {
            Ok(frame) => read.push(frame.expect(
                "the frame straddled a poll tick and the session must survive it: a torn read \
                 shows up here as a decrypt failure",
            )),
            Err(_elapsed) => {
                ticks += 1;
                assert!(
                    ticks <= 6,
                    "six poll ticks and still no whole frame: the reader lost the bytes it had \
                     already taken and is now reading ciphertext as a length prefix"
                );
            }
        }
    }

    dialing.await.expect("the writing task");

    assert!(
        ticks >= 1,
        "the poll timeout never fired, so this run never exercised a cancelled read"
    );
    assert_eq!(
        read,
        vec![b"first".to_vec(), b"second".to_vec()],
        "both frames, in order, decrypted under a transport whose nonce never skipped"
    );
}

/// **This node's key file never exists half-written.**
///
/// It was created and then written, two steps, so a crash, a full disk or a
/// SIGKILL between them left a 0-byte `tcr-node.key`. That file is not
/// repairable by this program: `load_or_mint` refuses it ("holds 0 bytes,
/// expected exactly 32") on every boot afterwards, and minting refuses a path
/// that already exists, by design, so the Mac has no identity and no way to
/// mint one until somebody deletes a file nothing told them about.
///
/// The instrument is a reader spinning on the path while the key is minted,
/// which is the same thing a boot on the next start is: it records the length
/// of the FIRST version of the file it manages to see. Run over many mints so
/// the window, which is microseconds wide, is actually sampled.
///
/// Watched red by restoring the create-then-write writer: the reader sees a
/// 0-byte key file.
#[test]
fn a_node_key_file_is_never_visible_before_its_bytes() {
    const MINTS: usize = 200;
    let mut seen_empty = 0_usize;
    let mut sampled = 0_usize;

    for _ in 0..MINTS {
        let dir = tempfile::tempdir().expect("a temp dir");
        let key_path = tcr::peer::id::private_key_path(dir.path());
        let start = Arc::new(std::sync::Barrier::new(2));
        let watcher_start = Arc::clone(&start);
        let watched = key_path.clone();
        // The first length this reader manages to see, and whether it saw the
        // file at all: a run that never sampled proves nothing, so it is
        // counted separately.
        let watcher = std::thread::spawn(move || {
            watcher_start.wait();
            for _ in 0..200_000 {
                if let Ok(meta) = std::fs::metadata(&watched) {
                    return Some(meta.len());
                }
            }
            None
        });

        start.wait();
        tcr::peer::id::NodeKey::load_or_mint(dir.path()).expect("a keypair is minted");
        if let Some(first) = watcher.join().expect("the watching thread finishes") {
            sampled += 1;
            if first != 32 {
                seen_empty += 1;
            }
        }
        assert_eq!(
            std::fs::read(&key_path).expect("the key file reads").len(),
            32,
            "the key that was minted is 32 bytes whatever the reader saw"
        );
    }

    assert!(
        sampled > 0,
        "the reader never caught the file at all, so this run measured nothing"
    );
    assert_eq!(
        seen_empty, 0,
        "a reader caught the key file at {seen_empty} of {sampled} sampled mints holding \
         something other than the 32 bytes: that file is refused on every later boot and \
         nothing overwrites it"
    );
}

/// Nothing is left beside the key files when minting succeeds.
///
/// Staging the bytes elsewhere and publishing them is how the write above
/// became atomic, and the failure that fix brings with it is an orphan: a file
/// holding this node's private static key, under a name nobody looks at, with
/// no owner to clean it up.
#[test]
fn minting_leaves_only_the_two_key_files() {
    let dir = tempfile::tempdir().expect("a temp dir");
    tcr::peer::id::NodeKey::load_or_mint(dir.path()).expect("a keypair is minted");

    let mut names: Vec<String> = std::fs::read_dir(dir.path())
        .expect("the config dir reads")
        .filter_map(|entry| Some(entry.ok()?.file_name().to_string_lossy().to_string()))
        .collect();
    names.sort();
    assert_eq!(
        names,
        ["tcr-node.key", "tcr-node.pub"],
        "a staged copy of the private key is still a copy of the private key"
    );
}

/// **A responder sizes its first read by the pattern it is running, not by the
/// Noise transport limit.**
///
/// `accept_handshake` read message 1 through the unbounded reader, which sizes
/// its buffer from the peer's own two-byte prefix: a stranger who writes
/// `0xFFFF` and then nothing made this node allocate 65 535 bytes for a frame
/// it was about to refuse. It was latent because the production listener reads
/// its own first frame through the bounded reader; this function is the one a
/// second caller would reach for.
///
/// The instrument is the REFUSAL's own wording, which says where it happened:
/// before the allocation, on the length prefix, rather than after a frame was
/// read and measured.
///
/// Watched red by putting `read_frame(stream)` back: the oversized prefix is
/// accepted, 65 535 bytes are allocated, and the refusal that follows is the
/// pattern check, which says "are not a `Noise_XX`… message 1".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_responders_first_read_is_bounded_by_its_own_pattern() {
    use tokio::io::AsyncWriteExt as _;
    let refusal_for = |promised: usize, handshake: Handshake| async move {
        let (mut responder, mut caller) = tokio::io::duplex(8192);
        let mut framed = u16::try_from(promised)
            .expect("the prefix fits")
            .to_be_bytes()
            .to_vec();
        // The prefix PROMISES more than the body carries, which is the whole
        // attack: two bytes and then nothing.
        framed.resize(2 + 8, 0);
        caller.write_all(&framed).await.expect("write the prefix");
        caller.flush().await.expect("flush");
        drop(caller);

        let err =
            noise::accept_handshake(&mut responder, &[9_u8; 32], handshake, &[], |_offered| {
                Err(PinRefusal::NotPinned {
                    offered: "no pinned key in this test".to_string(),
                })
            })
            .await
            .expect_err("a first frame that promises more than the pattern has is refused");
        format!("{err:#}")
    };

    let refusal = refusal_for(65_535, Handshake::Pair).await;
    assert!(
        refusal.contains("refused BEFORE allocating"),
        "the refusal has to happen on the length prefix, which is the only place it costs \
         nothing: {refusal}"
    );

    // An `IK` message 1 is 96 bytes and an `XX` one is not: a responder running
    // `XX` will not size a buffer for the longest pattern this node speaks
    // either.
    let refusal = refusal_for(noise::IK_MESSAGE_1_LEN, Handshake::Pair).await;
    assert!(
        refusal.contains("refused BEFORE allocating"),
        "the bound is this pattern's own message 1, not the largest any pattern has: \
         {refusal}"
    );

    // The positive control: the length this pattern DOES accept gets past the
    // bound and is refused for what it is, not for how long it is.
    let refusal = refusal_for(noise::IK_MESSAGE_1_LEN, Handshake::Return).await;
    assert!(
        !refusal.contains("refused BEFORE allocating"),
        "a message 1 of exactly this pattern's length is read, so the refusal above is the \
         bound and not a reader that refuses everything: {refusal}"
    );
}
