use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use axum::body::Body;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::Response;
use tcr_peer_wire::{Lease, PeerId, StreamHeader, StreamKind};

use crate::peer::config::PeerStore;
use crate::peer::config::id::NodeKey;
use crate::peer::noise::{self, Handshake, recv_json, send_json};
use crate::peer::serve::fallback::Ask;

use super::frame::{SERVE_FLOW, ServeAck, ServeReply, recv_body, send_body, serve_request_from};
use super::policy::serve_is_allowed_for_path;
use crate::peer::config::node_key_dir;
use crate::peer::tunnel::dial::dial_peer_reaching;

/// Open one SERVE to a lender and return its answer.
///
/// Refuses before opening anything when [`serve_is_allowed_for_path`] says no,
/// and scrubs before writing the first frame. The lease is the borrower's
/// cached copy; the lender's ledger is what actually decides.
///
/// `None` is "not me" in every case a lease could not be spent, no address that
/// answers, `disclose` not granted here, a refusal frame, because the caller's
/// next rung is the honest 429 this proxy already has and that is a better
/// answer than anything invented here. A protocol failure mid-stream IS an
/// error: it is not the same fact as "this lender said no".
///
/// The node key comes from the peers file's own directory, the way
/// `tcr peer id` resolves it (`src/main.rs:868`), so a test that points
/// `--peers` at a temp dir points this at the same one.
///
/// # `headers` is the borrower's own, and it is scrubbed here
///
/// [`Ask`] carries none today, and `crate::peer::serve::fallback`'s module doc says why: the
/// only code that strips a client credential sits 215 lines downstream of the
/// seam an [`Ask`] is built at. So the production caller hands in an empty map,
/// and this parameter exists for the two cases an empty map cannot serve, the
/// day something at the seam does hold headers, and a test that puts a
/// credential in scope so [`scrub_client_credentials`]'s removal is a
/// MEASUREMENT rather than an absence over an empty map. It was
/// `&HeaderMap::new()` inline, which made the scrub on this path untestable and
/// therefore unwatched.
///
/// [`scrub_client_credentials`]: super::scrub_client_credentials
pub async fn open_serve(
    lender: &PeerId,
    lease: &Lease,
    ask: &Ask<'_>,
    headers: &HeaderMap,
    store: &PeerStore,
) -> Result<Borrowed> {
    let deadline = Duration::from_millis(store.file().borrow_timeout_ms);
    open_serve_within(lender, lease, ask, headers, store, deadline).await
}

/// What one borrow from one lender came to.
///
/// **Three outcomes and not two, because the third one costs money.** This was
/// `Option<Response>`: `None` meant "not me", and a borrow that hit the
/// deadline answered it, so the caller offered the SAME body to the next
/// lender with a fresh request id. A lender that took the request and was
/// merely slow therefore had it executed and billed on its account, and again
/// on another peer's.
///
/// So the fact the caller needs is not "did an answer come back" but "did the
/// body cross". Once it has, this Mac cannot know whether the request ran, and
/// the one safe answer is to stop: no other lender, and no local re-send.
pub enum Borrowed {
    /// The lender answered, and this is its answer.
    Served(Response),
    /// Nothing crossed to this lender: no address that answers, `disclose` not
    /// granted here, a refusal frame, or a deadline reached before the body
    /// was written. The next lender may be asked.
    NotThisLender,
    /// The body crossed and no usable answer came back. The request may have
    /// been executed upstream, so it is never sent again, by anybody.
    DeliveredUnknown,
}

impl Borrowed {
    /// The answer, when there is one. For the callers that only ever ask
    /// "did this borrow produce a response".
    pub fn served(self) -> Option<Response> {
        match self {
            Self::Served(response) => Some(response),
            Self::NotThisLender | Self::DeliveredUnknown => None,
        }
    }
}

/// [`open_serve`] under an explicit deadline.
///
/// Exists so the deadline is a VALUE rather than a constant read inside the
/// function: a gate can then prove that a lender which accepts and never
/// answers releases the client, in a test that finishes in a fraction of a
/// second instead of ten, and the operator's own setting has somewhere to
/// arrive.
///
/// A deadline reached is never an error: "this lender did not answer in time"
/// is a fact about this lender, while an `Err` would be read as a protocol
/// failure and logged as one. WHICH outcome it is depends on one thing, whether
/// the body had already crossed when the clock ran out: see [`Borrowed`].
pub async fn open_serve_within(
    lender: &PeerId,
    lease: &Lease,
    ask: &Ask<'_>,
    headers: &HeaderMap,
    store: &PeerStore,
    deadline: Duration,
) -> Result<Borrowed> {
    // OUTSIDE the deadline, and first: the path refusal is unconditional and
    // decides without touching the network, so putting it inside a timeout
    // would only make it possible for a clock to change its answer.
    if !serve_is_allowed_for_path(ask.path) {
        tracing::debug!(
            path = ask.path,
            "peer serve: refused before opening anything, this path is never relayed"
        );
        return Ok(Borrowed::NotThisLender);
    }
    // THE FLAG THE DEADLINE READS. `timeout` cancels `borrow_once` at an await
    // point and takes its return value with it, so "had the body crossed?"
    // cannot be answered from what it returns. It is written by the one line
    // that writes the body frame, and read here by both the timeout arm and
    // the error arm.
    let delivered = AtomicBool::new(false);
    match tokio::time::timeout(
        deadline,
        borrow_once(lender, lease, ask, headers, store, &delivered),
    )
    .await
    {
        Ok(Ok(outcome)) => Ok(outcome),
        // A protocol failure AFTER the body crossed is the same fact as a
        // deadline after it: this Mac does not know whether the request ran,
        // so it is not sent again. Reported at warn rather than returned as an
        // `Err`, because an `Err` is what the caller reads as "ask the next
        // lender".
        Ok(Err(err)) if delivered.load(Ordering::SeqCst) => {
            tracing::warn!(
                peer = %lender.display(),
                error = %err,
                path = ask.path,
                "peer serve: the borrowed request was delivered and the exchange then \
                 failed; it is NOT offered to another lender, because it may have run"
            );
            Ok(Borrowed::DeliveredUnknown)
        }
        Ok(Err(err)) => Err(err),
        Err(_) if delivered.load(Ordering::SeqCst) => {
            tracing::warn!(
                peer = %lender.display(),
                deadline_ms = deadline.as_millis(),
                path = ask.path,
                "peer serve: the borrowed request was delivered and the lender did not \
                 answer within the deadline; it is NOT offered to another lender, \
                 because it may have run"
            );
            Ok(Borrowed::DeliveredUnknown)
        }
        Err(_) => {
            tracing::warn!(
                peer = %lender.display(),
                deadline_ms = deadline.as_millis(),
                path = ask.path,
                "peer serve: the lender did not take the borrow within the deadline; \
                 nothing crossed, so the client is released and the next lender may \
                 be asked"
            );
            Ok(Borrowed::NotThisLender)
        }
    }
}

/// The borrow itself, from the first socket to the last body byte, everything
/// [`open_serve_within`]'s one deadline covers. See that function.
async fn borrow_once(
    lender: &PeerId,
    lease: &Lease,
    ask: &Ask<'_>,
    headers: &HeaderMap,
    store: &PeerStore,
    delivered: &AtomicBool,
) -> Result<Borrowed> {
    store.reload_if_changed();
    let Some(row) = store.row(lender) else {
        return Ok(Borrowed::NotThisLender);
    };
    if !row.allow.allow_disclose {
        tracing::debug!(
            peer = %lender.display(),
            "peer serve: this Mac has not granted `disclose` to that peer, so its requests \
             stay here"
        );
        return Ok(Borrowed::NotThisLender);
    }

    let request_id = crate::peer::config::id::random_id()?;
    let request = serve_request_from(ask, headers, lease.lease_id, request_id)?;

    let key = NodeKey::load_or_mint(&node_key_dir(store))
        .context("peer serve: this node has no keypair to open a SERVE with")?;
    // `dial_peer_reaching` and not `dial_peer`: a lender whose own addresses
    // have all gone stale is still reachable through a Mac this node may ask
    // to carry, and the handshake below is unchanged either way, it runs
    // against the LENDER's pinned key over whatever stream came back, which is
    // what makes a forwarder blind.
    let Some(mut stream) = dial_peer_reaching(&row, store).await else {
        return Ok(Borrowed::NotThisLender);
    };
    let mut session = noise::dial_handshake(
        &mut stream,
        key.secret_bytes(),
        Handshake::Return,
        Some(&lender.0),
        None,
    )
    .await
    .context("peer serve: the handshake with the lender failed")?;

    let header = StreamHeader {
        kind: StreamKind::Serve,
        target: None,
        via: Vec::new(),
        hops_remaining: 1,
        request_id,
    };
    send_json(&mut stream, &mut session, &header).await?;
    send_json(&mut stream, &mut session, &request).await?;

    // THE ACK, AND WHY THE BODY WAITS FOR IT. Every refusal on the lender's
    // half happens after it has read this request frame: its header gate, its
    // path, method, version and request-id gates, and its lease. All of them
    // close the stream without an answer. A borrower that had already written
    // the body could not tell any of them from a lender that took the request
    // and died with it, so it told its client the request may have been billed
    // and skipped every remaining lender, over a request nothing had sent
    // anywhere. See [`ServeAck`].
    let ack: ServeAck = match recv_json(&mut stream, &mut session).await {
        Ok(ack) => ack,
        Err(err) => {
            // NOT an `Err` out of this function: "this lender answered nothing"
            // is a fact about the lender, and the body is still here. A build
            // older than [`SERVE_FLOW`] reads exactly like this, because it is
            // waiting for a body frame this one will not send until it has been
            // acknowledged, so the version is named here rather than guessed
            // at.
            tracing::debug!(
                peer = %lender.display(),
                error = %err,
                flow = SERVE_FLOW,
                "peer serve: the lender did not acknowledge taking this request, so it has \
                 none of it; a lender that speaks an older SERVE flow reads the same way"
            );
            return Ok(Borrowed::NotThisLender);
        }
    };
    if let ServeAck::Refused { refusal } = ack {
        tracing::info!(
            peer = %lender.display(),
            refusal = ?refusal,
            "peer serve: the lender refused this relayed request before its body was sent"
        );
        return Ok(Borrowed::NotThisLender);
    }

    send_body(&mut stream, &mut session, &ask.body).await?;
    // THE LINE THAT MAKES THIS REQUEST UNREPEATABLE. Everything above can fail
    // with nothing having left this Mac; from here on the lender holds the
    // body and may already have put it on its own account, so no later failure
    // may be reported as "not me". See [`Borrowed`].
    delivered.store(true, Ordering::SeqCst);

    let reply: ServeReply = recv_json(&mut stream, &mut session).await?;
    match reply {
        ServeReply::Refused { refusal } => {
            tracing::info!(
                peer = %lender.display(),
                refusal = ?refusal,
                "peer serve: the lender refused this relayed request"
            );
            // A REFUSAL IS "NOT ME" EVEN THOUGH THE BODY CROSSED, and it is
            // the one case where that is safe: the lender answers
            // `ServeReply::Refused` from `handle_serve_on` before it sends
            // anything upstream, so the request provably did not run. Every
            // other post-delivery outcome is `DeliveredUnknown`.
            Ok(Borrowed::NotThisLender)
        }
        ServeReply::Served {
            status,
            headers,
            body_bytes,
        } => {
            let body = recv_body(&mut stream, &mut session).await?;
            if body.len() != body_bytes {
                bail!(
                    "peer serve: the lender promised {body_bytes} body bytes and sent {}",
                    body.len()
                );
            }
            Ok(Borrowed::Served(response_from(status, &headers, body)?))
        }
    }
}

/// Rebuild an HTTP response from what the lender answered.
///
/// Hop-by-hop response headers are the lender's connection's business, not this
/// one's, so they are dropped here the way `src/proxy.rs` drops them on the way
/// back from upstream.
fn response_from(status: u16, headers: &[(String, String)], body: Vec<u8>) -> Result<Response> {
    let mut builder = Response::builder().status(
        StatusCode::from_u16(status)
            .with_context(|| format!("peer serve: {status} is not a status code"))?,
    );
    for (name, value) in headers {
        if crate::proxy::is_response_skip(name) {
            continue;
        }
        let Ok(name) = HeaderName::try_from(name.as_str()) else {
            continue;
        };
        let Ok(value) = HeaderValue::from_str(value) else {
            continue;
        };
        builder = builder.header(name, value);
    }
    builder
        .body(Body::from(body))
        .context("peer serve: could not build the response for the borrower's client")
}
