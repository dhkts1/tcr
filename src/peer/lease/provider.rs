//! The borrower's side: asking a lender for a lease. The `crate::peer::serve::fallback`
//! provider that asks is [`crate::peer::serve::PeerLeaseProvider`].

use anyhow::{Context as _, Result};
use tcr_peer_wire::{Control, Lease, LeaseRequest, PeerId, StreamHeader, StreamKind};

use super::hand::read_handed_bearers;
use crate::peer::config::id::random_id;
use crate::peer::config::{PeerRow, PeerStore};
use crate::peer::tunnel::dial;

/// Ask one lender for a lease, over a CONTROL stream.
///
/// The answer IS the offer: one round trip, no negotiation, because a fraction
/// changes on every request and a second trip would be spent on a stale number.
/// `None` is every "no" a borrower can do nothing about, no address that
/// answers, a refusal, and an error is a protocol failure, which is a different
/// fact.
///
/// **The lender's half of this exchange is one match arm that does not exist
/// yet.** `listener::serve_control` answers `Hello` and `Ping` and refuses
/// everything phase 4 owns, so a real `Control::LeaseRequest` reaching a merged
/// build is refused there, not by [`Ledger::grant`](super::Ledger::grant). Both halves exist; the arm
/// that joins them belongs in `src/peer/listener.rs`.
pub async fn request_lease(
    store: &PeerStore,
    lender: &PeerId,
    ask: &LeaseRequest,
) -> Result<Option<Lease>> {
    store.reload_if_changed();
    let Some(row) = store.row(lender) else {
        return Ok(None);
    };
    if !row.allow.allow_disclose {
        return Ok(None);
    }
    // The CONTROL stream a lease is asked over takes the same ways home the
    // SERVE stream does, forwarded hop included: a lender this node can only
    // reach through a friend must be askable, or the borrow stops one frame
    // before the stream that would have worked.
    let Some(mut stream) = dial::dial_peer_reaching(&row, store).await else {
        return Ok(None);
    };
    let key =
        crate::peer::config::id::NodeKey::load_or_mint(&crate::peer::config::node_key_dir(store))
            .context("peer lease: this node has no keypair to ask for a lease with")?;
    let mut session = crate::peer::noise::dial_handshake(
        &mut stream,
        key.secret_bytes(),
        crate::peer::noise::Handshake::Return,
        Some(&lender.0),
        None,
    )
    .await
    .context("peer lease: the handshake with the lender failed")?;

    let header = StreamHeader {
        kind: StreamKind::Control,
        target: None,
        via: Vec::new(),
        hops_remaining: 1,
        request_id: random_id()?,
    };
    crate::peer::noise::send_control(&mut stream, &mut session, &header).await?;
    crate::peer::noise::send_control(&mut stream, &mut session, &Control::LeaseRequest(*ask))
        .await?;
    let answer: Control = crate::peer::noise::recv_control(&mut stream, &mut session).await?;
    match answer {
        Control::LeaseGrant(grant) => {
            if let Some(refusal) = grant.refusal {
                tracing::info!(
                    peer = %lender.display(),
                    refusal = ?refusal,
                    "peer lease: the lender refused to grant a lease"
                );
            }
            if let Some(lease) = grant.lease {
                // The BORROWER half. The owner follows a hand-mode
                // grant with the bearer on THIS session and renews it there, so
                // the session outlives this function: it is handed to a reader
                // task rather than dropped with the frame still in flight,
                // which is what made `HandedTokens::put` a function with no
                // production caller.
                //
                // Not awaited here, and the reason is the serve-mode borrow:
                // the borrower cannot know which mode funded its lease (the
                // mode is the LENDER's own policy and never crosses the wire),
                // so a blocking read for a frame that may never come would put
                // a timeout on the front of every borrow. The reader waits
                // instead, and a hand-mode borrow that arrives before its
                // bearer falls through to the serve path, which is what a
                // borrower did on every build before this one.
                tokio::spawn(read_handed_bearers(stream, session, lease, *lender));
            }
            Ok(grant.lease)
        }
        other => {
            tracing::warn!(
                peer = %lender.display(),
                answer = ?other,
                "peer lease: the lender answered something other than a grant"
            );
            Ok(None)
        }
    }
}

/// The lenders this node may ask, in the order to ask them.
///
/// # The filter is what it always was, the ORDER is new
///
/// Both halves used to sit inside [`FallbackProvider::try_serve`]'s loop as a
/// `continue`, and the loop walked the peers file in file order. A lender this
/// Mac can open a socket to and one it can only reach by spending a third
/// Mac's bytes were therefore tried in whatever order an operator happened to
/// pair them in.
///
/// Sorting is the whole change and the key is one bit, has this row an address
/// of its own. A row with none is still asked, last: that is the case
/// [`dial::has_a_way_back`] exists for, a lender reachable only because a
/// mutual friend will carry, and also a lender that parked
/// a carrier at that friend because nothing can dial it at all
/// ([`crate::peer::tunnel::ReverseDesk`]). Asking it first would put a
/// forwarder's bytes and a forwarder's latency in front of a socket this node
/// could have opened itself.
///
/// The sort is stable, so within each group the peers file's own order stands
/// and an operator who ordered their file meant it.
pub fn lenders_in_ask_order(store: &PeerStore) -> Vec<PeerRow> {
    let mut rows: Vec<PeerRow> = store
        .peers()
        .into_iter()
        .filter(|row| row.allow.allow_disclose)
        // `has_a_way_back` and not `has_endpoint`: a lender whose row carries
        // no address at all is still reachable when this node holds
        // `allow.carry` on a Mac that can forward to it, and a filter that
        // read only the row would skip exactly the lender a forwarder exists
        // for.
        .filter(|row| dial::has_a_way_back(row, store))
        .collect();
    rows.sort_by_key(|row| u8::from(!row.has_endpoint()));
    rows
}

/// Did this failure happen BEFORE anything left this Mac?
///
/// The one question that separates "ask the next lender" from "tell the client
/// the outcome is unknown", asked on the hand-mode path where the request is
/// sent by this Mac's own `reqwest` client rather than over a peer stream.
///
/// The error arrives as an [`anyhow::Error`] with context wrapped round it, so
/// the chain is walked for the `reqwest::Error` underneath rather than the
/// string being matched: a connect failure and a resolver failure are the two
/// shapes where no byte was sent, and they are exactly the two `src/proxy.rs`
/// separates out on the direct path with the same two predicates. Anything
/// else, a reply that never arrived, a body that died mid-stream, may have run
/// upstream and is never offered to another lender.
pub fn nothing_left_this_mac(err: &anyhow::Error) -> bool {
    err.chain()
        .filter_map(|cause| cause.downcast_ref::<reqwest::Error>())
        .any(|cause| cause.is_connect() || crate::proxy::is_offline_error(cause))
}
