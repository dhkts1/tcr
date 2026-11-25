use anyhow::{Result, bail};
use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};
use tcr_peer_wire::{LeaseRefusal, MAX_FRAME_BYTES, PROTO_VERSION};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::fallback::Ask;
use crate::peer::noise;

use super::policy::{
    SERVE_METHOD, lender_forwards_header, scrub_client_credentials, serve_is_allowed_for_path,
};

/// How much body fits in ONE frame, which is now a chunk size and no longer a
/// ceiling on a body.
///
/// [`MAX_FRAME_BYTES`] is the whole frame a `u16` prefix can describe, and a
/// Noise transport message pays a 16-byte authentication tag inside it.
///
/// It used to be the ceiling on a whole body, and that was the defect: every
/// borrowed answer over 64 KiB, which any long completion is and which Claude
/// Code's streaming answers always are, was bought on the lender's account,
/// debited, and handed back as a 502 with no content. A body now travels as a
/// run of frames this size terminated by an empty one ([`send_body`],
/// [`recv_body`]), so the size a body may be is [`MAX_RELAYED_BODY_BYTES`] and
/// this is only how much of it one frame carries.
pub const MAX_BODY_BYTES: usize = MAX_FRAME_BYTES - 16;

/// The ceiling on a whole relayed body, request or answer, across every frame
/// it takes.
///
/// A bound is still needed, because the frames arrive before anything has said
/// how many there will be and a peer that keeps sending them would otherwise
/// grow this process's memory without limit. It is the size of a very large
/// completion rather than the size of one frame, and a body over it is refused
/// by name with both numbers.
pub const MAX_RELAYED_BODY_BYTES: usize = 32 * 1024 * 1024;

/// The SERVE stream's own flow version, which is NOT [`PROTO_VERSION`].
///
/// The two answer different questions and move on different clocks:
/// [`PROTO_VERSION`] is what a node advertises in discovery and records at
/// pairing, and bumping it to describe a change to one stream would re-date
/// every pinned row on the mesh. This is the order of frames on a SERVE stream
/// and nothing else.
///
/// **1 is the flow with no ack**: header, request, body, reply. **2 is this
/// one**: header, request, [`ServeAck`], body, reply. The difference is not
/// cosmetic and cannot be negotiated silently, a borrower that writes the body
/// before the ack has already made its request unrepeatable, which is the whole
/// defect the ack exists to fix. So a lender REFUSES any other flow by name
/// (see [`handle_serve_on`](crate::peer::serve::handle_serve_on)) rather than guessing at the order, and a borrower
/// that reads a closed stream where the ack should be says the same thing in
/// its log.
pub const SERVE_FLOW: u16 = 2;

/// The metadata frame of a relayed request. **The body is not in it**, it
/// travels as the next frame, raw, because a `Vec<u8>` through JSON is three to
/// four times its own size and this stream's frames are `u16`-bounded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServeRequest {
    /// Which lease pays for this request.
    pub lease_id: u128,
    /// One request, one id, for the whole path, what makes a retried or
    /// diamond-delivered relay debit once. The same id as the
    /// [`StreamHeader::request_id`] that opened the stream.
    pub request_id: u128,
    /// The HTTP method, as the borrower's client sent it.
    pub method: String,
    /// The already query-stripped request path.
    pub path: String,
    /// The query string the borrower's client sent, without its `?`, when it
    /// sent one.
    ///
    /// Apart from [`Self::path`] because the lender's own path gates match on
    /// the path, and a query string that could decide one of those is a query
    /// string that decides routing. The two are put back together only on the
    /// URL the lender builds for its own proxy.
    ///
    /// `#[serde(default)]` so a frame from a build written before this field
    /// existed still parses, and reads as the client having sent no query,
    /// which is exactly what that build meant.
    #[serde(default)]
    pub query: Option<String>,
    /// Header names and values, **after the borrower's scrub**. Pairs rather
    /// than a map because a request may legitimately repeat a name.
    pub headers: Vec<(String, String)>,
    /// How many bytes the body frames that follow this one carry in total.
    pub body_bytes: usize,
    /// The wire version the borrower speaks, so a lender can refuse a frame it
    /// would have to guess at.
    pub proto: u16,
    /// The order of frames the borrower is going to use. See [`SERVE_FLOW`].
    ///
    /// `#[serde(default)]` so a frame from a build written before this field
    /// existed parses and reads 0, which is what lets the lender refuse it BY
    /// NAME with both numbers rather than fail to deserialize and close with a
    /// message about JSON. 0 is never a flow this build speaks.
    #[serde(default)]
    pub flow: u16,
}

/// The lender's answer to "may I send you this request", and the frame that
/// makes a borrowed request repeatable until it is sent.
///
/// It sits between the request frame and the body, and it is the ONLY thing
/// that moves a borrow from "nothing has left this Mac" to "this may already
/// have run". Before it existed the borrower wrote the body first, so a lender
/// that refused at its header gate, or at any of `handle_serve_on`'s own gates,
/// produced a no-retry 502 telling the client its request may have been billed
/// when nothing had been sent anywhere.
///
/// A lender writes it once its gates have passed and BEFORE it sends anything
/// upstream. A stream that closes, or goes quiet, where this frame belongs is a
/// lender that took nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServeAck {
    /// Send the body. This lender has taken the request.
    Accepted,
    /// Refused before the body, in the lease vocabulary the borrower already
    /// reads. Nothing crossed, so the next lender may be asked.
    Refused { refusal: LeaseRefusal },
}

/// The lender's answer to one relayed request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServeReply {
    /// Served on the lender's own account. The body follows as a run of frames
    /// terminated by an empty one, and `body_bytes` is what they add up to.
    Served {
        status: u16,
        headers: Vec<(String, String)>,
        body_bytes: usize,
    },
    /// Refused, in the lease vocabulary the borrower already reads.
    /// [`LeaseRefusal::InFlightFull`] is the one that means "retry in a moment"
    /// rather than "stop asking".
    ///
    /// Every refusal a lender decides BEFORE the body is a [`ServeAck`] now.
    /// What is left here is the one that can only be known after it: a body
    /// whose length does not match what the request frame promised, which the
    /// lender refuses without sending anything upstream, so it is still "not
    /// this lender" and still provably unrun.
    Refused { refusal: LeaseRefusal },
}

/// Build the metadata frame for one relayed request, scrubbing as it goes.
///
/// `headers` is whatever the caller holds. It is scrubbed IN this function
/// rather than by its caller, so there is no order of operations for a future
/// caller to get wrong: a credential cannot reach a [`ServeRequest`] because the
/// only constructor removes it. [`Ask`] carries no headers at all today, which
/// is the outer half of the same defence, this is the half that survives the
/// day something at the seam does hold them.
pub fn serve_request_from(
    ask: &Ask<'_>,
    headers: &HeaderMap,
    lease_id: u128,
    request_id: u128,
) -> Result<ServeRequest> {
    if !serve_is_allowed_for_path(ask.path) {
        bail!(
            "peer serve: {} is a path a relay must never carry, so no frame is built for it",
            ask.path
        );
    }
    // The METHOD, refused here and not rewritten. This function used to write
    // the literal `SERVE_METHOD` into the frame whatever the client had sent,
    // which is how a `GET` came to be POSTed on somebody else's account. The
    // seam refuses a non-POST before it builds an `Ask` at all
    // (`src/proxy.rs`), so this is the second half of that defence and the half
    // that holds for a caller written later: the only constructor of a
    // `ServeRequest` cannot produce one whose method the lender will refuse.
    if !ask.method.eq_ignore_ascii_case(SERVE_METHOD) {
        bail!(
            "peer serve: {} is not a method a relayed request may carry ({SERVE_METHOD} or \
             nothing); refused rather than rewritten",
            ask.method
        );
    }
    // The whole body, not one frame of it: a body is chunked over as many
    // frames as it takes ([`send_body`]), so what is refused here is a request
    // larger than this stream carries at all rather than one larger than a
    // `u16` prefix. A real request over the old one-frame cap was a routine
    // shape, and refusing it meant the mesh declined exactly the long
    // conversations it was built for.
    if ask.body.len() > MAX_RELAYED_BODY_BYTES {
        bail!(
            "peer serve: this request's body is {} bytes and a relayed request carries {}; \
             refused rather than truncated",
            ask.body.len(),
            MAX_RELAYED_BODY_BYTES
        );
    }

    let mut scrubbed = headers.clone();
    scrub_client_credentials(&mut scrubbed);
    // AND THEN NARROWED TO THE ALLOW LIST, which is what actually crosses the
    // host boundary. The scrub above removes the names that are credentials;
    // this keeps only the names a lender would forward anyway
    // ([`LENDER_FORWARDED_HEADERS`]), so a header a borrower invented is not
    // sent to another Mac merely because nobody has classified it yet. The
    // lender applies the same list to what it receives: two readers of one
    // rule, on either side of the wire, and the borrower's is the one that
    // decides what leaves this machine.
    let headers = scrubbed
        .iter()
        .filter(|(name, _)| lender_forwards_header(name.as_str()))
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_string(), value.to_string()))
        })
        .collect();

    Ok(ServeRequest {
        lease_id,
        request_id,
        // The client's own method, which the check above has proved is the one
        // method this build relays. Written from the `Ask` rather than from the
        // constant so the frame says what the client asked for and a future
        // build that relays a second method has one place to widen.
        method: ask.method.to_string(),
        path: ask.path.to_string(),
        query: ask.query.map(str::to_string),
        headers,
        body_bytes: ask.body.len(),
        proto: PROTO_VERSION,
        flow: SERVE_FLOW,
    })
}

/// Write a body as a run of frames terminated by an empty one.
///
/// **The terminator is what makes this readable at all.** The reader cannot
/// count frames, the count is not on the wire ahead of them, and it must not
/// trust the byte total in the request or reply frame either: that figure is a
/// promise the sender makes and the reader CHECKS, so using it to decide when
/// to stop reading would make it unfalsifiable. An empty frame is a frame, it
/// costs a 16-byte tag, and a body of zero bytes is exactly the terminator on
/// its own.
pub(super) async fn send_body<S>(
    stream: &mut S,
    session: &mut noise::PeerSession,
    body: &[u8],
) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    for chunk in body.chunks(MAX_BODY_BYTES) {
        noise::send_encrypted(stream, &mut session.transport, chunk).await?;
    }
    noise::send_encrypted(stream, &mut session.transport, &[]).await
}

/// Read a body written by [`send_body`].
///
/// Bounded by [`MAX_RELAYED_BODY_BYTES`] as it goes rather than at the end: a
/// peer that keeps sending frames is refused when it crosses the bound, not
/// after this process has already held the bytes.
pub(super) async fn recv_body<S>(
    stream: &mut S,
    session: &mut noise::PeerSession,
) -> Result<Vec<u8>>
where
    S: AsyncRead + Unpin,
{
    let mut body = Vec::new();
    loop {
        let frame = noise::recv_encrypted(stream, &mut session.transport).await?;
        if frame.is_empty() {
            return Ok(body);
        }
        if body.len() + frame.len() > MAX_RELAYED_BODY_BYTES {
            bail!(
                "peer serve: this body is past {MAX_RELAYED_BODY_BYTES} bytes and is still \
                 arriving; refused rather than held"
            );
        }
        body.extend_from_slice(&frame);
    }
}
