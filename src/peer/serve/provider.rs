//! The borrower's `crate::fallback` provider, [`PeerLeaseProvider`], and the
//! hand-mode serve it falls to, [`serve_on_handed_bearer`].

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result};
use axum::http::HeaderMap;
use axum::response::Response;
use futures::future::BoxFuture;
use tcr_peer_wire::{Lease, LeaseRequest, LeaseUnit, PeerId, Window};

use crate::fallback::{Ask, FallbackProvider};
use crate::peer::config::PeerStore;
use crate::peer::lease::{
    handed_tokens, lenders_in_ask_order, nothing_left_this_mac, report_handed_spend,
    reported_utilization, request_lease,
};
use crate::peer::serve;

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
    /// lender's [`MAX_LEASES_PER_PEER`](crate::peer::lease::MAX_LEASES_PER_PEER).
    ///
    /// A `tokio::sync::Mutex` and not the `std` one beside it, because this is
    /// the one lock in this type that is deliberately held ACROSS an await, the
    /// ask itself, a dial, a handshake and two frames. `self.leases` is still
    /// the `std` lock and is still never held across an await.
    ///
    /// Bounded by the peers file: one entry per lender ever asked, and a peers
    /// file is an operator-written list, not peer-driven input. It is not
    /// pruned, for [`Ledger::live_for`](crate::peer::lease::Ledger::live_for)'s reason, an entry dropped on one path
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
    /// [`Ledger::restored_from`](crate::peer::lease::Ledger::restored_from) prints.
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
    /// [`Ledger::persist`](crate::peer::lease::Ledger::persist)'s reason: the caller is on a live client's answer
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
/// lender (see [`clamp_to_grant`](crate::peer::lease::clamp_to_grant)), so these are an opening position and not a
/// promise, which is why they are written as an ask rather than as a policy.
const DEFAULT_ASK_FRACTION: f64 = 0.5;
const DEFAULT_ASK_TTL_S: u32 = 600;
const DEFAULT_ASK_MAX_INFLIGHT: u8 = 4;

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
/// Serve one borrowed request LOCALLY, on the bearer the owner handed over.
///
/// This is the half of the rule that makes `hand` mode different from
/// `serve` mode at all: the bytes never touch the owner's Mac, so the request
/// leaves from this node's own client and the owner never sees the plaintext.
/// In `serve` mode the same lease would have gone through
/// [`serve::open_serve`] and out of the owner's machine instead.
///
/// `client` is supplied by the caller rather than built here, because the
/// caller is the only code that knows which client this node's egress policy
/// says to use, and because a test points one at a fake origin with
/// `reqwest::ClientBuilder::resolve` (the escape `src/peer/egress.rs` already
/// documents: it overrides DNS only, so TLS still validates the real hostname).
///
/// `Ok(None)` for "no live handed bearer for this lease", which is not an
/// error: the owner may not have pushed one yet, or the lease may have ended.
/// The fallback ladder's next rung answers the client, and an unauthenticated
/// call to upstream never happens.
pub async fn serve_on_handed_bearer(
    upstream_base: &str,
    client: &reqwest::Client,
    ask: &Ask<'_>,
    lease_id: u128,
    window: Window,
    now_ms: i64,
) -> Result<Option<Response>> {
    // The client's own headers, narrowed to the names a lender forwards, for
    // the reason `try_serve` hands them to `open_serve`: this request goes to
    // the API, and the API answers 400 to one with no `anthropic-version`. Hand
    // mode sends from THIS Mac, so the narrowing that `serve_request_from` does
    // on the wire is done here instead; `build_handed_bearer_headers` then puts
    // the owner's bearer on and strips what a pooled request never carries.
    let mut forwarded = HeaderMap::new();
    for (name, value) in &ask.headers {
        if serve::lender_forwards_header(name.as_str()) {
            forwarded.append(name.clone(), value.clone());
        }
    }
    let Some(headers) = crate::proxy::build_handed_bearer_headers(&forwarded, lease_id, now_ms)
    else {
        return Ok(None);
    };

    // `set_path` on a parsed base, never `format!`, for the reason
    // `peer::serve::serve_on_own_account` gives at length: a path that does not
    // begin with `/` turns the authority into userinfo and sends this request
    // to a host the client named.
    let mut url =
        reqwest::Url::parse(upstream_base).context("peer hand: the upstream base is not a URL")?;
    url.set_path(ask.path);
    url.set_query(ask.query);

    let mut send = client.request(
        reqwest::Method::from_bytes(ask.method.as_bytes())
            .context("peer hand: the client's method is not a method")?,
        url,
    );
    for (name, value) in &headers {
        send = send.header(name.clone(), value.clone());
    }
    if !ask.body.is_empty() {
        send = send.body(ask.body.clone());
    }

    let response = send
        .send()
        .await
        .context("peer hand: the request on the handed bearer did not reach upstream")?;
    let status = response.status().as_u16();
    let head: Vec<(String, String)> = response
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_string(), value.to_string()))
        })
        .collect();
    // WHAT THIS ANSWER COST THE OWNER, REPORTED BACK. A hand-mode lease was
    // debited by nobody: the request leaves from this Mac, so the owner never
    // sees it, and the only evidence of the spend is the rate-limit header on
    // this answer. `report_handed_spend` turns two successive answers into the
    // rise and sends it up the lease's own control session. See
    // [`HandedMeter`].
    report_handed_spend(lease_id, reported_utilization(&head, window));

    let body = response
        .bytes()
        .await
        .context("peer hand: the upstream response body did not read")?;

    // Built here rather than through `peer::serve`'s private `response_from`:
    // that function belongs to the relay path, this node is not relaying, and
    // widening its visibility would make one helper answer for two paths that
    // have different reasons to drop a header.
    let mut builder = Response::builder().status(
        axum::http::StatusCode::from_u16(status)
            .with_context(|| format!("peer hand: {status} is not a status code"))?,
    );
    for (name, value) in &head {
        // Hop-by-hop response headers describe the connection this node just
        // made, not the one it is answering, exactly as the proxy drops them on
        // the way back from upstream.
        if crate::proxy::is_response_skip(name) {
            continue;
        }
        let Ok(name) = axum::http::HeaderName::try_from(name.as_str()) else {
            continue;
        };
        let Ok(value) = axum::http::HeaderValue::from_str(value) else {
            continue;
        };
        builder = builder.header(name, value);
    }
    builder
        .body(axum::body::Body::from(body))
        .context("peer hand: could not build the response for this node's own client")
        .map(Some)
}
