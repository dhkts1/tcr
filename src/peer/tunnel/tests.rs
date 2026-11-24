use super::*;
use std::time::Duration;

use crate::peer::id::NodeKey;

/// A transport whose `poll_shutdown` does nothing, the way the `tailscale`
/// crate's TCP stream behaves at 0.6.1 (`stream.rs:319-336`): the far end
/// never sees a FIN until the whole socket is dropped.
struct NoHalfClose(DuplexStream);

impl AsyncRead for NoHalfClose {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl AsyncWrite for NoHalfClose {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// Two ends of one Return session over a duplex pipe, neither of which
/// can half-close.
async fn pumps_with_no_half_close() -> (NoiseStream, NoiseStream) {
    let responder_dir = tempfile::tempdir().expect("tempdir");
    let dialler_dir = tempfile::tempdir().expect("tempdir");
    let responder = NodeKey::load_or_mint(responder_dir.path()).expect("a responder key");
    let dialler = NodeKey::load_or_mint(dialler_dir.path()).expect("a dialler key");
    let responder_static = responder.id().0;
    let pinned = dialler.id();
    let responder_secret = *responder.secret_bytes();

    let (mut near, mut far) = tokio::io::duplex(64 * 1024);
    let accepting = tokio::spawn(async move {
        let session = noise::accept_handshake(
            &mut far,
            &responder_secret,
            noise::Handshake::Return,
            &[],
            move |_remote| Ok(pinned),
        )
        .await
        .expect("the responder's handshake");
        (far, session)
    });
    let near_session = noise::dial_handshake(
        &mut near,
        dialler.secret_bytes(),
        noise::Handshake::Return,
        Some(&responder_static),
        None,
    )
    .await
    .expect("the dialler's handshake");
    let (far, far_session) = accepting.await.expect("the responder task");
    (
        NoiseStream::start(NoHalfClose(near), near_session),
        NoiseStream::start(NoHalfClose(far), far_session),
    )
}

/// **Over a transport with no half-close, the far end still sees the end
/// of the stream at once, and the near pump stops at once too.**
///
/// The end-of-stream frame ([`send_end_of_stream`]) is what carries it.
/// Watch it fail by making the pump skip the frame (or ignore an empty
/// one on receipt): the far read then waits for the near pump's
/// [`HALF_CLOSE_DRAIN`], ten seconds, and the two-second deadline fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_end_of_a_carry_crosses_a_transport_with_no_half_close() {
    let (mut near, mut far) = pumps_with_no_half_close().await;

    near.write_all(b"hello").await.expect("write");
    near.shutdown().await.expect("shut the near side");

    let mut got = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), far.read_to_end(&mut got))
        .await
        .expect("the far end sees the end of the stream, not a hang")
        .expect("read");
    assert_eq!(got, b"hello");

    // The far pump ended on the frame and said its own end, so the near
    // pump is not left draining for ten seconds.
    tokio::time::timeout(Duration::from_secs(2), near.finish())
        .await
        .expect("the near pump stops without waiting out its drain")
        .expect("a clean end");
    tokio::time::timeout(Duration::from_secs(2), far.finish())
        .await
        .expect("the far pump stops")
        .expect("a clean end");
}
