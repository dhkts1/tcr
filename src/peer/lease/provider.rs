//! The borrower's side: asking a lender for a lease, and [`PeerLeaseProvider`].

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result};
use axum::response::Response;
use futures::future::BoxFuture;
use tcr_peer_wire::{
    Control, Lease, LeaseRequest, LeaseUnit, PeerId, StreamHeader, StreamKind, Window,
};

use super::hand::{handed_tokens, read_handed_bearers, serve_on_handed_bearer};
use crate::fallback::{Ask, FallbackProvider};
use crate::peer::config::{PeerRow, PeerStore};
use crate::peer::id::random_id;
use crate::peer::serve;

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
    let Some(mut stream) = serve::dial_peer_reaching(&row, store).await else {
        return Ok(None);
    };
    let key = crate::peer::id::NodeKey::load_or_mint(&crate::peer::config::node_key_dir(store))
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
    serve::send_control(&mut stream, &mut session, &header).await?;
    serve::send_control(&mut stream, &mut session, &Control::LeaseRequest(*ask)).await?;
    let answer: Control = serve::recv_control(&mut stream, &mut session).await?;
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

/// The peer-lease implementation of the proxy's fallback seam.
///
/// Implementation ONE of [`FallbackProvider`]; the api-key backend is slot two
/// behind the same trait.
pub struct PeerLeaseProvider {
    /// The peers file this provider reads its policy from. Held as a path and
    /// opened per ask rather than as a long-lived store, because this seam is
    /// reached only when the whole local fleet could not serve a request, a
    /// rare event by construction, and a path cannot go stale.
    peers_path: PathBuf,
    /// Leases this node has been granted, by lender. A CACHED HINT and every
    /// surface that renders it says so: the lender's ledger is authoritative,
    /// which is why a refusal frame drops the row here rather than arguing.
    leases: Mutex<HashMap<PeerId, Lease>>,
    /// Where that cache is written so something other than this process can
    /// read it: the `borrowed` section of the peers file's own
    /// `peer-state.json` ([`crate::peer::state::BorrowedRow`]).
    ///
    /// Derived from `peers_path` rather than passed in, by the same
    /// [`crate::peer::state::peer_state_path`] every other peer surface uses,
    /// so one `--peers` argument still points the whole peer surface at one
    /// directory and a test cannot write the operator's real file.
    state_path: PathBuf,
    /// One gate per lender, held for the whole of an ASK, so that N concurrent
    /// first requests against one lender mint ONE lease between them instead of
    /// N.
    ///
    /// Measured before it existed: five concurrent borrows against a lender
    /// granting `max_inflight: 2` were all five SERVED and none refused, because
    /// each of the five missed the cache, released the cache lock, asked, and
    /// got a lease of its own, so the cap that binds concurrency within one
    /// lease never bound anything across them, and the only ceiling left was the
    /// lender's [`MAX_LEASES_PER_PEER`].
    ///
    /// A `tokio::sync::Mutex` and not the `std` one beside it, because this is
    /// the one lock in this type that is deliberately held ACROSS an await, the
    /// ask itself, a dial, a handshake and two frames. `self.leases` is still
    /// the `std` lock and is still never held across an await.
    ///
    /// Bounded by the peers file: one entry per lender ever asked, and a peers
    /// file is an operator-written list, not peer-driven input. It is not
    /// pruned, for [`Ledger::live_for`]'s reason, an entry dropped on one path
    /// and not another is how a borrower ends up with two gates for one lender
    /// and no single-flight at all.
    ask_gates: tokio::sync::Mutex<HashMap<PeerId, Arc<tokio::sync::Mutex<()>>>>,
    /// Where a HAND-mode request goes and the client it goes with: decision
    /// 15's whole difference, which is that this node dials upstream itself.
    ///
    /// A field rather than a constant because a test has to point it at a fake
    /// origin, and pointing it there is the only way to observe WHICH node
    /// spent the token. `None` means this node will not serve a hand-mode
    /// request at all and every lease falls through to the `serve` path, which
    /// is the behaviour of every build before row 15.
    hand_egress: Option<(String, reqwest::Client)>,
}

impl PeerLeaseProvider {
    /// A provider that reads its policy from `peers_path`, with the borrowed
    /// leases the last process left on disk already in its cache.
    ///
    /// The restore is here and not in a separate call because there is no
    /// instant between the two where a half-built provider would be correct:
    /// a borrower that answered one request off an empty cache would ask its
    /// lender for a second lease it already holds. Expired rows are dropped by
    /// [`crate::peer::state::load`] on the way in and counted in the
    /// `restored=N expired=M` line below, the same shape the lender's
    /// [`Ledger::restored_from`](super::Ledger::restored_from) prints.
    ///
    /// An unreadable state file is an EMPTY cache and a warning, never a
    /// refusal to build: every lease in it is re-askable, and a borrower that
    /// refused to borrow over a cache file would have traded a real service
    /// for a durability promise.
    pub fn new(peers_path: PathBuf) -> Self {
        let state_path = crate::peer::state::peer_state_path(&peers_path);
        let mut leases: HashMap<PeerId, Lease> = HashMap::new();
        let now_ms = crate::now_ms();
        match crate::peer::state::load(&state_path, now_ms) {
            Ok(state) => {
                let rows = state.borrowed.len();
                for row in state.borrowed {
                    leases.insert(row.lender, row.lease);
                }
                tracing::info!(
                    path = %state_path.display(),
                    restored = leases.len(),
                    // A second row for one lender is the same lender's newer
                    // lease overwriting its older one, which is what the cache
                    // holds anyway. Counted so the two numbers cannot be read
                    // as agreeing when they do not.
                    rows,
                    "peer lease: borrowed leases restored"
                );
            }
            Err(err) => tracing::warn!(
                path = %state_path.display(),
                error = %err,
                "peer lease: the borrowed leases could not be read, so this borrower starts \
                 with none and asks every lender again"
            ),
        }
        Self {
            peers_path,
            leases: Mutex::new(leases),
            state_path,
            ask_gates: tokio::sync::Mutex::new(HashMap::new()),
            // Built here so a production build can serve a hand-mode lease with
            // no further wiring, and `no_proxy` for the reason
            // `peer::serve::serve_on_own_account` gives: an environment proxy
            // variable must not silently route a request that is spending
            // somebody else's account through a third host.
            hand_egress: crate::http_client::builder()
                .no_proxy()
                .build()
                .inspect_err(|err| {
                    tracing::warn!(
                        error = %err,
                        "peer lease: no client for hand-mode requests, so every lease falls \
                         back to serving through the owner"
                    );
                })
                .ok()
                .map(|client| (crate::config::default_upstream(), client)),
        }
    }

    /// Point hand-mode requests at `base` through `client`.
    ///
    /// The seam a test needs, and the seam an egress policy would use: a client
    /// carrying a `reqwest::ClientBuilder::resolve` override reaches a fake
    /// origin on loopback without any DNS or TLS pretence, which is how the
    /// gate observes which node spent the token.
    pub fn with_hand_egress(mut self, base: String, client: reqwest::Client) -> Self {
        self.hand_egress = Some((base, client));
        self
    }

    /// Is there a live handed bearer for this lease right now?
    ///
    /// The borrower's whole test for "is this lease in hand mode": the owner
    /// backs a `hand` grant with a pushed token, and a mode with no token is a
    /// request this node could not pay for.
    fn handed_bearer_is_live(lease_id: u128) -> bool {
        let now_ms = crate::now_ms();
        handed_tokens()
            .lock()
            .is_ok_and(|store| store.bearer(lease_id, now_ms).is_some())
    }

    /// Serve one ask on the handed bearer, from this node.
    async fn serve_handed(&self, ask: &Ask<'_>, lease: &Lease) -> Result<Option<Response>> {
        let Some((base, client)) = self.hand_egress.as_ref() else {
            return Ok(None);
        };
        serve_on_handed_bearer(
            base,
            client,
            ask,
            lease.lease_id,
            lease.window,
            crate::now_ms(),
        )
        .await
    }

    /// The leases this node holds right now, as the state file spells them.
    ///
    /// The one read-only view of the cache: a surface that wants the borrower's
    /// side reads this rather than the state file, so it cannot disagree with
    /// the process that is spending them.
    pub fn borrowed(&self) -> Vec<crate::peer::state::BorrowedRow> {
        let Ok(cached) = self.leases.lock() else {
            return Vec::new();
        };
        cached
            .iter()
            .map(|(lender, lease)| crate::peer::state::BorrowedRow {
                lease: *lease,
                lender: *lender,
            })
            .collect()
    }

    /// Write the cache to the `borrowed` section of the state file.
    ///
    /// Called when the SET of live borrowed leases changes, one granted, one
    /// dropped after a refusal, and never per relayed request: the borrower's
    /// `spent` is the lender's own figure, kept in the lender's ledger, so
    /// there is nothing on this side that moves per request to write down.
    ///
    /// A failed write is LOGGED and never propagated, for
    /// [`Ledger::persist`]'s reason: the caller is on a live client's answer
    /// path, and a borrower that refused a request it can serve because a
    /// cache file is read-only would be the worse trade. The warning is how an
    /// operator learns their `tcr peer ls` will read `until: null`.
    fn persist_borrowed(&self) {
        let rows = self.borrowed();
        if let Err(err) = crate::peer::state::save_borrowed(&self.state_path, &rows) {
            tracing::warn!(
                path = %self.state_path.display(),
                error = %err,
                "peer lease: the borrowed leases could not be written, so no other process \
                 can see what this Mac is borrowing"
            );
        }
    }

    /// The cached lease for this lender while it is live and for this window,
    /// or `None`.
    ///
    /// Its own function because [`Self::lease_for`] reads the cache TWICE, once
    /// on the fast path and once behind the ask gate, and two hand-written
    /// copies of "live, and for this window" are two chances to disagree about
    /// what a usable cached lease is.
    fn cached_lease(&self, lender: &PeerId, window: Window) -> Option<Lease> {
        let now = crate::now_ms();
        let cached = self.leases.lock().ok()?;
        let lease = cached.get(lender)?;
        if lease.expires_at_ms > now && lease.window == window {
            Some(*lease)
        } else {
            None
        }
    }

    /// The lease to spend on this lender: the cached one while it is live,
    /// otherwise one freshly asked for.
    ///
    /// The ask is SINGLE-FLIGHTED per lender (see [`Self::ask_gates`]): the
    /// first caller to miss the cache holds that lender's gate for the whole
    /// round trip and the others wait on it, then read the lease it cached. Any
    /// other arrangement lets a borrower mint one lease per concurrent request
    /// and spend `max_inflight` once per lease, which is the same as having no
    /// cap.
    async fn lease_for(&self, store: &PeerStore, lender: &PeerId, window: Window) -> Option<Lease> {
        if let Some(lease) = self.cached_lease(lender, window) {
            return Some(lease);
        }
        let gate = {
            let mut gates = self.ask_gates.lock().await;
            Arc::clone(
                gates
                    .entry(*lender)
                    .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
            )
        };
        // Held until this function returns. The map's own lock is already gone:
        // holding it here would serialise asks against DIFFERENT lenders too,
        // which is a fleet-wide queue behind one sleeping laptop.
        let _asking = gate.lock().await;
        // Behind the gate, because the caller that just released it is the one
        // that filled the cache, this read is where the other four of five
        // concurrent asks get their lease.
        if let Some(lease) = self.cached_lease(lender, window) {
            return Some(lease);
        }
        let ask = LeaseRequest {
            window,
            unit: LeaseUnit::Fraction(DEFAULT_ASK_FRACTION),
            ttl_s: DEFAULT_ASK_TTL_S,
            max_inflight: DEFAULT_ASK_MAX_INFLIGHT,
        };
        let granted = match request_lease(store, lender, &ask).await {
            Ok(lease) => lease?,
            Err(err) => {
                tracing::warn!(
                    peer = %lender.display(),
                    error = %err,
                    "peer lease: asking for a lease failed"
                );
                return None;
            }
        };
        if let Ok(mut cached) = self.leases.lock() {
            cached.insert(*lender, granted);
        }
        // AFTER the guard is dropped: `persist_borrowed` takes the same lock to
        // read the rows out, and taking it twice on one thread is a deadlock.
        self.persist_borrowed();
        Some(granted)
    }
}

/// What a borrower asks for when nothing has told it to ask for less: half a
/// window, ten minutes, four at once. Every one of the three is clamped by the
/// lender (see [`clamp_to_grant`]), so these are an opening position and not a
/// promise, which is why they are written as an ask rather than as a policy.
const DEFAULT_ASK_FRACTION: f64 = 0.5;
const DEFAULT_ASK_TTL_S: u32 = 600;
const DEFAULT_ASK_MAX_INFLIGHT: u8 = 4;

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
/// [`serve::has_a_way_back`] exists for, a lender reachable only because a
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
        .filter(|row| serve::has_a_way_back(row, store))
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

/// The answer a borrower gives its client when a borrowed request was
/// delivered and its outcome is unknowable.
///
/// **502 and `x-should-retry: false`**, the shape `src/proxy.rs` already
/// answers with when an attempt got past connect before failing: the request
/// may have run upstream, so a client that retries could pay for it twice. A
/// 429 would be worse than useless here, Claude Code retries one by design.
///
/// The one thing this must never be is `None`: the ladder's next rung is a
/// retryable answer, and every rung below this point ends in the client
/// sending the same body again.
fn delivered_unknown_response() -> Response {
    let payload = serde_json::json!({
        "type": "error",
        "error": {
            "type": "proxy_error",
            "message": "This request was sent to a peer's account and no answer came back \
                        before the deadline. It may have been served there, so it was NOT \
                        sent again: retrying may pay for it twice.",
        },
    });
    let mut response = Response::new(axum::body::Body::from(payload.to_string()));
    *response.status_mut() = axum::http::StatusCode::BAD_GATEWAY;
    let headers = response.headers_mut();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    headers.insert(
        "x-should-retry",
        axum::http::HeaderValue::from_static("false"),
    );
    response
}

impl FallbackProvider for PeerLeaseProvider {
    fn name(&self) -> &'static str {
        "peer-lease"
    }

    /// The path refusal is FIRST and it is unconditional, before any lease is
    /// looked at, so a credential path cannot reach a lender even by way of a
    /// lease that would otherwise have served it. Everything after it answers
    /// `None`, which costs the caller only the next rung of the ladder.
    fn try_serve<'a>(&'a self, ask: &'a Ask<'a>) -> BoxFuture<'a, Option<Response>> {
        Box::pin(async move {
            if !serve::serve_is_allowed_for_path(ask.path) {
                tracing::debug!(
                    path = ask.path,
                    "not relaying {}, falling back to the local path",
                    serve::refused_path_kind(ask.path)
                );
                return None;
            }

            let store = match PeerStore::open(&self.peers_path) {
                Ok(store) => store,
                Err(err) => {
                    // Surfaced, never swallowed: a peers file that cannot be
                    // read is not the same fact as a node with no peers, and
                    // the ladder's next rung answers the client either way.
                    tracing::warn!(
                        error = %err,
                        path = %self.peers_path.display(),
                        "peer lease: the peers file is unreadable, so nothing is borrowed"
                    );
                    return None;
                }
            };

            // A request with no model-scoped window can only be served by an
            // untiered lease, and `7d_oi` is the only model-scoped window
            // upstream reports at all, so an untiered window is what is asked
            // for, and a model-scoped ask would be inventing a bucket upstream
            // does not report for anything but Fable.
            let window = Window::SevenDay;

            // Filtered and ordered in one place ([`lenders_in_ask_order`]): a
            // lender this Mac can dial is asked before one that needs a third
            // Mac to carry, and a row with no way back at all is not asked.
            for row in lenders_in_ask_order(&store) {
                let Some(lease) = self.lease_for(&store, &row.node, window).await else {
                    continue;
                };
                // The `hand`/`serve` fork, and the ONLY place a borrowed request
                // chooses between the two modes. A handed bearer for this
                // lease means the owner granted it in `hand` mode and pushed a
                // token, so the request is served from here and never crosses
                // to the owner; with no handed bearer the `serve` path below is
                // unchanged, byte for byte, which is what every grant written
                // before row 15 already meant.
                //
                // The borrower reads the MODE off the presence of a token
                // rather than off a field it was told: a mode the owner
                // asserted but never backed with a bearer would otherwise
                // become a request this node cannot pay for.
                if Self::handed_bearer_is_live(lease.lease_id) {
                    match self.serve_handed(ask, &lease).await {
                        Ok(Some(response)) => return Some(response),
                        Ok(None) => {}
                        // A CONNECT failure is the one error here that is not
                        // a delivery. `reqwest` reports "the socket never
                        // opened" and "the reply never arrived" as the same
                        // type, and this arm read both as delivered, so a
                        // lender whose handed bearer pointed at an upstream
                        // this Mac could not even reach ended the ladder with
                        // a no-retry 502 over a request that had not left the
                        // box. The direct path separates the two with the same
                        // predicate (`src/proxy.rs`, `is_connect` plus the
                        // resolver check), and this is that predicate.
                        Err(err) if nothing_left_this_mac(&err) => {
                            tracing::warn!(
                                peer = %row.node.display(),
                                error = %err,
                                "peer lease: the hand-mode request never reached an \
                                 upstream, so nothing was served and the next lender may \
                                 be asked"
                            );
                        }
                        Err(err) => {
                            // NOT `continue`. A hand-mode request leaves from
                            // THIS Mac, on the owner's bearer, so an error
                            // here is a request that may already be running
                            // upstream. Offering the same body to the next
                            // lender is how one POST is executed twice, which
                            // is the defect `Borrowed::DeliveredUnknown`
                            // exists for on the serve path.
                            tracing::warn!(
                                peer = %row.node.display(),
                                error = %err,
                                "peer lease: the hand-mode request failed after it left \
                                 this Mac; it is NOT offered to another lender"
                            );
                            return Some(delivered_unknown_response());
                        }
                    }
                    continue;
                }
                // The ASK's headers, which are the client's own with every
                // credential already removed by `Ask::scrubbed`. This used to be an
                // empty map, on the reading that an `Ask` could hold nothing
                // safe: the lender then sent a bearer and nothing else to the
                // API, which answers 400 to a request with no
                // `anthropic-version`, and the borrower served that 400 to its
                // client. `serve_request_from` narrows this to
                // `LENDER_FORWARDED_HEADERS` before anything crosses the host
                // boundary, and scrubs again on the way.
                match serve::open_serve(&row.node, &lease, ask, &ask.headers, &store).await {
                    Ok(serve::Borrowed::Served(response)) => return Some(response),
                    // **THE LADDER STOPS HERE.** The body crossed to that
                    // lender and no answer came back, so this Mac cannot know
                    // whether the request ran. Asking the next lender would
                    // execute and bill it a second time, and answering `None`
                    // would hand the client a 429 it is built to retry, which
                    // is the same double-send one hop further out. So the
                    // client is told the outcome is unknown, once.
                    Ok(serve::Borrowed::DeliveredUnknown) => {
                        return Some(delivered_unknown_response());
                    }
                    Ok(serve::Borrowed::NotThisLender) => {
                        // A refusal, or nothing reachable. The lease may be the
                        // stale half of it, so the hint is dropped and the next
                        // ask re-asks rather than spending a lease the lender
                        // has forgotten.
                        let dropped = match self.leases.lock() {
                            Ok(mut cached) => cached.remove(&row.node).is_some(),
                            Err(_) => false,
                        };
                        // Only when a row actually went: a refusal against a
                        // lender this node holds no lease for must not rewrite
                        // the state file on every relayed request.
                        if dropped {
                            self.persist_borrowed();
                        }
                    }
                    Err(err) => tracing::warn!(
                        peer = %row.node.display(),
                        error = %err,
                        "peer lease: the SERVE stream failed"
                    ),
                }
            }
            None
        })
    }
}
