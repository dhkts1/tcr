// ---------------------------------------------------------------------------
// The mesh harness: one lender and one borrower in this one test binary, on
// kernel ports.
//
// **The lender is the REAL listener now.** A harness used to stand in for the
// one line that joins the two halves, because `listener::serve_stream` matched
// `StreamKind::Serve` to a `bail!`. That arm is now landed, so
// [`spawn_lender`] hands a kernel-port `TcpListener` to
// `listener::serve_on_with` and the production dispatch is what answers: the
// gate, the per-frame row re-read, the refusal log and the handler are all the
// shipped code, not a copy of it here.
//
// [`spawn_capturing_lender`] is the one harness that remains, and it exists to
// see something the real listener deliberately never exposes: the DECRYPTED
// request frame. See its doc.
// ---------------------------------------------------------------------------

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tcr::log_err::LogErr as _;
use tcr::manager::Manager;
use tcr::peer::config::PeerStore;
use tcr::peer::id::NodeKey;
use tcr::peer::lease::Ledger;
use tcr::peer::listener::{self, LeaseServing, SessionContext};
use tcr::peer::noise;
use tcr::peer::serve::{self, WindowUtilization};
use tcr_peer_wire::{Control, Lease, LeaseGrant, LeaseUnit, StreamHeader, StreamKind, Window};
use tokio::net::TcpListener;

use super::fleet;

/// A utilization reader whose answers are scripted rather than measured.
///
/// `handle_serve_on` reads the lease's window twice around one relay, so a
/// two-element script is one request's before and after, and the debit that
/// results is a figure the test chose. Past the end of the script the last
/// answer repeats, so a harness written for one relay does not panic on a
/// second.
///
/// The real reader is `Manager`, which reads its accounts' quota; measuring
/// a real rise needs the fake upstream to move the lender's utilization
/// between two probes, which is a test of `update_from_headers` rather than
/// of the debit.
pub(crate) struct ScriptedUtilization {
    script: Vec<Vec<Option<f64>>>,
    taken: Mutex<usize>,
}

impl ScriptedUtilization {
    /// A reader that answers `script[0]`, then `script[1]`, and so on.
    pub(crate) fn new(script: Vec<Vec<Option<f64>>>) -> Self {
        assert!(!script.is_empty(), "a script with no reads answers nothing");
        Self {
            script,
            taken: Mutex::new(0),
        }
    }

    /// How many reads happened. The positive control on the instrument: a
    /// reader nobody called would satisfy a debit assertion by accident.
    pub(crate) fn reads_taken(&self) -> usize {
        *self.taken.lock().expect("scripted-utilization lock")
    }
}

impl WindowUtilization for ScriptedUtilization {
    fn read(&self, _window: Window) -> Vec<Option<f64>> {
        let mut taken = self.taken.lock().expect("scripted-utilization lock");
        let at = (*taken).min(self.script.len() - 1);
        *taken += 1;
        self.script[at].clone()
    }
}

/// Stand up the lender's REAL peer listener and serve SERVE streams on it.
///
/// `listener::serve_on_with` is the shipped accept loop, so what answers a
/// borrower here is the production handshake, the production stream gate,
/// the production per-frame row re-read and the production dispatch arm.
/// The only thing this function chooses is what a listener cannot derive:
/// the ledger, this node's own proxy base and the quota reader
/// ([`LeaseServing`]).
///
/// The pairing-window file is inside `key_dir` and does not exist, so this
/// test reads nothing under the operator's cache directory and cannot be perturbed by an
/// operator who has `tcr peer pair` open while the suite runs. A missing
/// file reads as a closed window, which is what a pinned-peer `IK` return
/// needs anyway.
pub(crate) async fn spawn_lender(
    key_dir: PathBuf,
    peers_path: PathBuf,
    ledger: Arc<Mutex<Ledger>>,
    upstream: String,
    utilization: Arc<dyn WindowUtilization>,
    manager: Arc<Manager>,
) -> SocketAddr {
    let listening = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the lender's peer listener");
    let addr = listening.local_addr().expect("lender peer addr");

    let key = NodeKey::load_or_mint(&key_dir).expect("the lender's node key");
    let store = PeerStore::open(&peers_path).expect("the lender's peers file");
    let state_path = key_dir.join("peer-state.json");
    let context = SessionContext::new(&key, store.path(), &state_path).with_lease_serving(Some(
        LeaseServing {
            ledger,
            upstream,
            utilization,
            manager,
        },
    ));

    tokio::spawn(async move {
        // The loop only ever ends by the listener failing, which in this
        // binary means the test finished and the socket was dropped.
        listener::serve_on_with(listening, context)
            .await
            .log_debug("serving a test peer listener");
    });
    addr
}

/// A lender that reads a borrow and answers nothing at all.
///
/// The shape of EVERY lender-side refusal that happens before an upstream
/// is reached: the header gate (`listener.rs`, `inspect` turned off while a
/// borrower still holds a lease), and each `bail!` in
/// `serve::handle_serve_on` (a wire version, a path, a method or a request
/// id this build will not serve). All of them close the stream with no
/// frame written back.
///
/// It reads the frames a lender reads and then goes silent, rather than
/// closing the socket, because a close is a RACE on loopback: the
/// borrower's own write can fail with the connection already gone, which
/// hides the defect behind the kernel's timing. A lender that simply never
/// answers is the same fact to the borrower and it is the same every run.
pub(crate) async fn spawn_lender_that_takes_nothing(
    key_dir: PathBuf,
    peers_path: PathBuf,
) -> SocketAddr {
    let listening = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the silent lender");
    let addr = listening.local_addr().expect("silent lender addr");
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listening.accept().await else {
                return;
            };
            let key_dir = key_dir.clone();
            let peers_path = peers_path.clone();
            tokio::spawn(async move {
                let key = NodeKey::load_or_mint(&key_dir).expect("the lender's node key");
                let store = PeerStore::open(&peers_path).expect("the lender's peers file");
                let rows = store.peers();
                let Ok(mut session) = noise::accept_handshake(
                    &mut stream,
                    key.secret_bytes(),
                    noise::Handshake::Return,
                    &[],
                    move |remote| noise::pin_check_rows(remote, &rows),
                )
                .await
                else {
                    return;
                };
                let Ok(header) =
                    noise::recv_control::<_, StreamHeader>(&mut stream, &mut session).await
                else {
                    return;
                };
                assert_eq!(header.kind, StreamKind::Serve);
                // The request frame, read exactly as a lender reads it, and
                // then nothing: no ack, no reply, and the stream held open
                // so the borrower's own deadline is what ends this.
                noise::recv_encrypted(&mut stream, &mut session.transport)
                    .await
                    .log_debug("reading the request frame");
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            });
        }
    });
    addr
}

/// Accept one connection the way a lender's responder does: a `Return`
/// handshake on the key in `key_dir`, authorized by `pin_check_rows`
/// against the rows in `peers_path` between message 1 and message 2, then
/// the stream header. `None` is a handshake or a header that failed, said
/// on stderr under `who`.
async fn accept_pinned(
    stream: &mut tokio::net::TcpStream,
    key_dir: &std::path::Path,
    peers_path: &std::path::Path,
    who: &str,
) -> Option<(noise::PeerSession, StreamHeader)> {
    let key = NodeKey::load_or_mint(key_dir).expect("the lender's node key");
    let store = PeerStore::open(peers_path).expect("the lender's peers file");
    let rows = store.peers();
    let mut session = match noise::accept_handshake(
        stream,
        key.secret_bytes(),
        noise::Handshake::Return,
        &[],
        move |remote| noise::pin_check_rows(remote, &rows),
    )
    .await
    {
        Ok(session) => session,
        Err(err) => {
            eprintln!("{who}: the handshake failed: {err:#}");
            return None;
        }
    };
    match noise::recv_control(stream, &mut session).await {
        Ok(header) => Some((session, header)),
        Err(err) => {
            eprintln!("{who}: no stream header: {err:#}");
            None
        }
    }
}

/// The rest of a SERVE stream once its request frame is read, answered the
/// way a lender answers: the ack (the borrower holds its body until it
/// arrives), the body and its empty terminator read, then a 200 carrying
/// [`fleet::CANNED`].
async fn answer_canned(stream: &mut tokio::net::TcpStream, session: &mut noise::PeerSession) {
    noise::send_control(stream, session, &serve::ServeAck::Accepted)
        .await
        .expect("the ack frame");
    noise::recv_encrypted(stream, &mut session.transport)
        .await
        .expect("the body frame");
    let end = noise::recv_encrypted(stream, &mut session.transport)
        .await
        .expect("the body terminator");
    assert!(end.is_empty(), "a body ends with an empty frame");
    let reply = serve::ServeReply::Served {
        status: 200,
        headers: vec![("content-type".to_string(), "application/json".to_string())],
        body_bytes: fleet::CANNED.len(),
    };
    noise::send_control(stream, session, &reply)
        .await
        .expect("the reply frame");
    noise::send_encrypted(stream, &mut session.transport, fleet::CANNED)
        .await
        .expect("the reply body");
    noise::send_encrypted(stream, &mut session.transport, b"")
        .await
        .expect("the reply body terminator");
}

/// Every DECRYPTED request frame the lender read, in order.
pub(crate) type CapturedFrames = Arc<Mutex<Vec<Vec<u8>>>>;

/// A lender that captures the decrypted [`serve::ServeRequest`] frame and
/// answers a canned reply, serving nothing.
///
/// # Why this is not the real listener
///
/// The borrower's scrub is the check that matters, and the only place its
/// effect is VISIBLE is the frame the lender decrypted. On the wire that
/// frame is Noise ciphertext, so an assertion there is an assertion about
/// randomness; inside the real listener it is a local variable nothing
/// exposes, and exposing it would mean a production seam that hands a
/// borrower's plaintext request to a caller, which is the opposite of what
/// this module is for.
///
/// So this harness stands exactly where `handle_serve_on` stands, reads the
/// same two frames off the same session, and keeps the first one's bytes.
/// That is what makes `a_relayed_frame_the_lender_receives_holds_no_
/// credential` a measurement: delete the scrub and the borrower's token is
/// in these bytes.
pub(crate) async fn spawn_capturing_lender(
    key_dir: PathBuf,
    peers_path: PathBuf,
    captured: CapturedFrames,
) -> SocketAddr {
    let listening = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the capturing lender");
    let addr = listening.local_addr().expect("capturing lender addr");
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listening.accept().await else {
                return;
            };
            let key_dir = key_dir.clone();
            let peers_path = peers_path.clone();
            let captured = Arc::clone(&captured);
            tokio::spawn(async move {
                // `noise::accept_handshake` (in `accept_pinned`) and NOT
                // `listener::accept_peer_session`, which this was the last
                // caller of. That function is the pre-decision-10 seam: it
                // admits an `XX` message 1 from ANY address while a
                // node-wide pairing window is open, which is exactly what
                // was replaced with a window keyed to ONE accepted
                // instance id. Nothing in production reached it: the
                // listener calls `accept_pairing_or_return`: so it was a
                // retired shape kept alive by one test, and this caller
                // had to go before that function could be deleted.
                //
                // Nothing is lost here. A borrower dials a lender with
                // `Handshake::Return` (`serve::open_serve`), the pattern
                // this harness only ever exercised, and the authorization
                // is the SAME one the production responder applies:
                // `pin_check_rows` against the peers file's own rows,
                // between message 1 and message 2, with nothing written
                // until it has answered.
                let Some((mut session, header)) =
                    accept_pinned(&mut stream, &key_dir, &peers_path, "capturing lender").await
                else {
                    return;
                };
                assert_eq!(header.kind, StreamKind::Serve);

                // The request frame, kept as the bytes that came out of the
                // Noise transport: before `serde_json` has had a chance to
                // drop a field this build does not know about.
                let frame = noise::recv_encrypted(&mut stream, &mut session.transport)
                    .await
                    .expect("the request frame");
                captured
                    .lock()
                    .expect("captured-frames lock")
                    .push(frame.clone());
                let request: serve::ServeRequest =
                    serde_json::from_slice(&frame).expect("the request frame parses");
                // Before the ack rather than after the reply: a panic in
                // this task is invisible to the test unless it stops the
                // reply the borrower is waiting on.
                assert_eq!(request.body_bytes, 2, "the fixture ask carries `{{}}`");
                answer_canned(&mut stream, &mut session).await;
            });
        }
    });
    addr
}

/// How many `Control::LeaseRequest` frames a lender has read.
pub(crate) type LeaseAsks = Arc<std::sync::atomic::AtomicUsize>;

/// A lender that COUNTS lease asks, grants every one of them, and serves
/// whatever is then relayed to it.
///
/// # Why not `spawn_lender`
///
/// The real listener is the right harness for what a lender DECIDES, and it
/// is the wrong one for how many times it was ASKED: the ask count lives in
/// the listener's own control arm as a local, and the nearest readable proxy
/// (how many leases the ledger minted), is one layer past the question. A
/// lender that refused an ask, or minted one lease for two asks, would read
/// identically there. So this stands where the control arm stands, counts
/// the decrypted `Control::LeaseRequest` frames, and answers a canned grant.
///
/// `answer_delay` is the whole instrument: it holds the first ask open long
/// enough that four more land inside it, which is the race
/// `PeerLeaseProvider::lease_for`'s ask gate exists to close. Without the
/// gate this counter reads five.
pub(crate) async fn spawn_counting_lease_lender(
    key_dir: PathBuf,
    peers_path: PathBuf,
    asks: LeaseAsks,
    answer_delay: std::time::Duration,
) -> SocketAddr {
    let listening = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the counting lender");
    let addr = listening.local_addr().expect("counting lender addr");
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listening.accept().await else {
                return;
            };
            let key_dir = key_dir.clone();
            let peers_path = peers_path.clone();
            let asks = Arc::clone(&asks);
            tokio::spawn(async move {
                let Some((mut session, header)) =
                    accept_pinned(&mut stream, &key_dir, &peers_path, "counting lender").await
                else {
                    return;
                };
                match header.kind {
                    StreamKind::Control => {
                        let control: Control = noise::recv_control(&mut stream, &mut session)
                            .await
                            .expect("the control frame");
                        std::assert_matches!(
                            control,
                            Control::LeaseRequest(_),
                            "this lender answers lease asks and nothing else"
                        );
                        asks.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        tokio::time::sleep(answer_delay).await;
                        let now_ms = tcr::now_ms();
                        let grant = LeaseGrant {
                            lease: Some(Lease {
                                lease_id: 0xc0_ffee,
                                window: Window::SevenDay,
                                unit: LeaseUnit::Fraction(0.20),
                                granted_at_ms: now_ms,
                                expires_at_ms: now_ms + 300_000,
                                spent: 0.0,
                                max_inflight: 2,
                                until: None,
                            }),
                            refusal: None,
                        };
                        noise::send_control(&mut stream, &mut session, &Control::LeaseGrant(grant))
                            .await
                            .expect("the grant frame");
                    }
                    StreamKind::Serve => {
                        let frame = noise::recv_encrypted(&mut stream, &mut session.transport)
                            .await
                            .expect("the request frame");
                        let _request: serve::ServeRequest =
                            serde_json::from_slice(&frame).expect("the request frame parses");
                        answer_canned(&mut stream, &mut session).await;
                    }
                    other => panic!("this lender takes no {other:?} stream"),
                }
            });
        }
    });
    addr
}
