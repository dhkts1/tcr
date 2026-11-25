use std::collections::VecDeque;
use std::future::Future;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use futures::stream::{FuturesUnordered, StreamExt as _};
use tcr_peer_wire::{Control, PeerId, StreamHeader, StreamKind, TunnelTarget};

use crate::peer::config::id::NodeKey;
use crate::peer::config::{Endpoint, Locator, PeerRow, PeerStore};
use crate::peer::noise::{self, Handshake, NoiseStream, PeerStream, recv_control, send_control};

use crate::peer::config::node_key_dir;
use crate::peer::state::peer_state_path;

/// How long the whole borrow may take before the client is released: the
/// connect, the handshake, both frames out, the lender's reply and its body.
///
/// **One deadline over the whole exchange, not one per hop.** The review's
/// MEDIUM was that this path had none anywhere, a bare `TcpStream::connect`,
/// an unbounded `dial_handshake` and an unbounded wait for the reply, on the
/// answer path of a live client's request, while the listener's own half
/// ([`crate::peer::listener::HANDSHAKE_TIMEOUT`]) and the blind-egress carry
/// ([`crate::peer::config::CARRY_SETUP_TIMEOUT`]) each carry one. Per-hop
/// deadlines would sum to a bound no caller can state; the client is waiting on
/// the total, so the total is what is bounded.
///
/// Ten seconds, and deliberately LONGER than the carry's five: a carry runs
/// after the direct path has already failed, while a borrow is this request's
/// first and only attempt at an answer, and a borrow that gave up before a
/// sleeping laptop's radio woke would spend a lease nobody could use.
///
/// # The operator's setting, and this constant's new job
///
/// [`crate::peer::config::PeerFile::borrow_timeout_ms`] is now the value
/// [`open_serve`] reads per call, off the [`PeerStore`](crate::peer::config::PeerStore) it is already handed,
/// so a lowered timeout takes effect without a restart, exactly like `via`.
/// This constant is what a fresh install starts at:
/// `PeerFile::borrow_timeout_ms`'s own serde default mirrors it, so a peers
/// file with no opinion behaves exactly as it did before that field existed.
///
/// [`open_serve`]: crate::peer::serve::open_serve
pub const BORROW_TIMEOUT: Duration = Duration::from_millis(10_000);

/// The order the endpoints on a row are tried in.
///
/// # Why the order is a function and not a `sort` inside the dial
///
/// It is the whole of this node's routing policy, and it is worth one thing a
/// test can call. Four keys decide it and they are stated once, each with its
/// reason, in [`crate::peer::reach::probe::order_endpoints`]: the kind of path the
/// operator asked for, then the loss ceiling, then the measured round trip,
/// then the newest observation.
///
/// The two rules below are what REMAINS when nothing has been measured. A path
/// no probe has answered on carries no loss figure and no round trip, so every
/// such path ties on the two middle keys and falls through to these two, which
/// is how a fleet that has never been probed dials exactly as it did before
/// probing existed:
///
/// - **Newest first**, because an endpoint is a record of where a peer
///   answered and the most recent observation is the best guess about now. The
///   row already keeps this order ([`PeerRow::observe_endpoint`]); re-stating
///   it here means a caller reading this function knows the order without
///   having to go and check that the writer still sorts.
/// - **`Via` last**, because a forwarded hop spends a THIRD machine's bytes and
///   needs its consent. A direct socket that works is always the better answer,
///   so a hop is what is left when nothing direct answered, never a peer
///   chosen because its timestamp happened to be newer.
pub fn dial_order(row: &PeerRow) -> Vec<Endpoint> {
    crate::peer::reach::probe::dial_order(row)
}

/// Dial the row's endpoints in [`dial_order`] and take the first that answers,
/// reporting WHICH endpoint that was.
///
/// The address is returned because the caller that re-dates an endpoint needs
/// it and the dial is the only thing that knows it: a boxed stream cannot be
/// asked for its peer address, and asking the row again would name the
/// endpoint this node tried first rather than the one that answered.
///
/// Endpoints are ROUTING ADVICE and nothing else: identity is re-proven by the
/// handshake against the pinned static key, so trying them in order costs
/// nothing but a connect timeout and buys a peer that moved.
pub async fn dial_peer_with_endpoint(row: &PeerRow) -> Option<(SocketAddr, PeerStream)> {
    dial_peer_within(row, borrow_timeout_default_ms()).await
}

/// [`BORROW_TIMEOUT`] in milliseconds, for the callers that have no peers file
/// in hand.
fn borrow_timeout_default_ms() -> u64 {
    u64::try_from(BORROW_TIMEOUT.as_millis()).unwrap_or(10_000)
}

/// How many connect attempts one endpoint's budget is divided between.
///
/// Three, because that is how many rendezvous ports a pair has in any one slot
/// ([`crate::peer::reach::accepted_ports`]): current, previous, next. The
/// recorded port is tried on the same per attempt bound, so an endpoint whose
/// host has gone away costs a bounded wait instead of the operating system's
/// own connect timeout, which on a route that blackholes is over a minute.
const DIAL_ATTEMPTS_PER_ENDPOINT: u32 = 3;

/// How long a dial waits for one direct candidate to answer before starting
/// the next one, without cancelling the one already in flight.
///
/// RFC 8305 section 5 ("Happy Eyeballs Version 2") names 250 milliseconds as
/// the default Connection Attempt Delay, and this is that figure, not a
/// number this codebase chose. It is deliberately not an operator knob: a
/// value this small is not a timeout an operator would ever want to reach
/// for, and exposing it would invite tuning a number that already has a
/// settled answer.
pub(super) const CONNECTION_ATTEMPT_DELAY: Duration = Duration::from_millis(250);

/// [`dial_peer_with_endpoint`] with the operator's borrow timeout handed in.
///
/// # What this adds over dialling the recorded endpoints
///
/// Nothing about the ORDER: that is [`dial_order`] and stays there, so a sort
/// key this function does not own can replace it without touching the loop
/// below. What is here is the second thing to try when an endpoint's recorded
/// PORT stops answering, which is the case the derived port
/// exists for: the peer is still at that address and its router mapping moved,
/// or it rebound. The pair's three rendezvous ports for the current slot are
/// tried against the same host, in [`crate::peer::reach::accepted_ports`]'s
/// order, so one slot of skew still meets.
///
/// # The bound
///
/// Each attempt gets `borrow_timeout_ms / DIAL_ATTEMPTS_PER_ENDPOINT`, so the
/// three rendezvous attempts against one host fit inside the timeout the
/// operator set for the borrow that is waiting on this dial. A borrower that
/// waited the full timeout on one dead port and never reached the port the
/// peer moved to would have spent the budget proving the endpoint it already
/// knew was wrong.
///
/// A process holding no port secret for this peer tries the recorded
/// endpoints and stops, exactly as this function did before: see
/// [`crate::peer::reach::rendezvous_ports`] for when that is.
pub async fn dial_peer_within(
    row: &PeerRow,
    borrow_timeout_ms: u64,
) -> Option<(SocketAddr, PeerStream)> {
    let per_attempt = per_attempt_bound(borrow_timeout_ms);
    let rendezvous = crate::peer::reach::rendezvous_ports(&row.node, now_unix_seconds());
    let mut candidates = Vec::new();
    for endpoint in dial_order(row) {
        if endpoint.direct_addr().is_none() {
            // Logged rather than dropped silently, because "the row had a way
            // back and nothing used it" is exactly the failure the forward-dial
            // path exists to end. A caller here cannot follow it:
            // a hop needs the forwarder's own row and this node's keypair, and
            // this entry point is handed neither. [`dial_peer_reaching`] is.
            tracing::debug!(
                peer = %row.node.display(),
                "peer serve: this row's next endpoint is a forwarded hop and this dial was \
                 handed no peers file to read the forwarder's row from; skipping it"
            );
            continue;
        }
        candidates.push(endpoint);
    }

    let bound = Duration::from_millis(borrow_timeout_ms.max(1));
    let race = run_direct_race(row, candidates, &rendezvous, per_attempt, bound).await;
    for endpoint in race.failed {
        crate::peer::reach::probe::cool_down(row.node, endpoint.locator, crate::now_ms());
    }
    let (endpoint, reached) = race.won?;
    crate::peer::reach::probe::clear_cooldown(row.node, endpoint.locator);
    Some(reached)
}

/// Each attempt's share of the borrow timeout. See
/// [`DIAL_ATTEMPTS_PER_ENDPOINT`].
fn per_attempt_bound(borrow_timeout_ms: u64) -> Duration {
    Duration::from_millis((borrow_timeout_ms / u64::from(DIAL_ATTEMPTS_PER_ENDPOINT)).max(1))
}

/// What is left of `deadline`, zero once it has passed.
///
/// [`Instant::saturating_duration_since`] rather than a plain subtraction: a
/// later stage that runs after the deadline nominally expired (a slow leading
/// race that still returned) gets a real zero budget instead of a panic or a
/// wrapped-around duration.
fn remaining(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

/// The leading direct race's share of the borrow deadline, as a divisor: the
/// race is bounded by `borrow_timeout_ms / LEADING_RACE_SHARE`, not the whole
/// deadline.
///
/// Not derived from the race's own mechanics, because the obvious derivation
/// reproduces the bug it exists to fix. Each candidate's own attempt is not
/// bounded by one `per_attempt`: [`try_direct_endpoint`] tries the recorded
/// port and then this pair's other [`DIAL_ATTEMPTS_PER_ENDPOINT`] rendezvous
/// ports, each up to `per_attempt`, and `per_attempt` is itself
/// `borrow_timeout_ms / DIAL_ATTEMPTS_PER_ENDPOINT`
/// ([`per_attempt_bound`]). Three attempts at a third of the deadline each is
/// the whole deadline, so one blackholing candidate already costs about the
/// whole `borrow_timeout_ms` on its own, and a bound built only from
/// candidate count and stagger delay -- `(n - 1) * CONNECTION_ATTEMPT_DELAY +
/// per_attempt` -- is smaller than that single-candidate cost. Using it as
/// the race's own external bound would cut a real, merely slow, candidate
/// off mid attempt rather than give a later step a turn, which is the exact
/// failure this exists to end. A flat named share of the deadline is the
/// honest choice instead: half, so the leading race still has room to catch
/// a real but slow answer, and half is left for the dead-drop retry and a
/// punch to have a real turn rather than chase a deadline that already
/// passed.
const LEADING_RACE_SHARE: u64 = 2;

/// One recorded socket, then this pair's rendezvous ports against the same
/// host, the whole of what "try this direct endpoint" means, in one place
/// because two dial entry points share it and a second copy of a retry loop is
/// a second answer to "how patient is a dial".
async fn try_direct_endpoint(
    row: &PeerRow,
    addr: SocketAddr,
    rendezvous: &[u16],
    per_attempt: Duration,
) -> Option<(SocketAddr, PeerStream)> {
    if let Some(stream) = connect_within(addr, per_attempt).await {
        return Some((addr, stream));
    }
    for port in rendezvous {
        let candidate = SocketAddr::new(addr.ip(), *port);
        if candidate == addr {
            // Already tried, as the recorded port.
            continue;
        }
        if let Some(stream) = connect_within(candidate, per_attempt).await {
            tracing::debug!(
                peer = %row.node.display(),
                addr = %candidate,
                "peer serve: the recorded port stopped answering and this pair's \
                 rendezvous port did"
            );
            return Some((candidate, stream));
        }
    }
    None
}

/// The two halves a candidate race learns: who won, and who was asked and
/// came back with nothing.
///
/// Both in one value rather than a bool plus an out-parameter: a caller that
/// only wrote the winner's outcome would have to invent its own second pass
/// over the candidates to find out which ones to cool down, and a second pass
/// invented at the call site is a second answer to a question this type
/// already has.
pub(super) struct RaceOutcome<C, T> {
    /// The candidate that answered first, with what its own attempt returned.
    pub(super) won: Option<(C, T)>,
    /// Candidates whose own attempt completed and returned nothing. A
    /// candidate still running when another won is not in here: it was
    /// dropped, not refused.
    pub(super) failed: Vec<C>,
}

/// Runs one attempt per candidate, starting the next `stagger` after the one
/// before it without waiting for an answer first, and returns the moment any
/// one attempt answers.
///
/// Every attempt still in flight at that moment is DROPPED, which is what
/// cancels it: `FuturesUnordered` holds each attempt's future and dropping
/// the whole race function's stack drops every future it owns. Nothing here
/// is ever `tokio::spawn`ed, because a spawned task keeps running, and
/// answering, after its handle is dropped, and "the rest are dropped" would
/// stop being true the moment one candidate outlived this function.
///
/// A candidate whose own attempt completes and returns [`None`] before
/// another candidate wins goes into [`RaceOutcome::failed`]; a candidate
/// still running when the race ends does not, because it was cancelled, not
/// refused.
///
/// Generic over the candidate (`C`), the attempt (`F`) and what it returns
/// (`T`), so the two production callers can hand this [`Endpoint`] values
/// without this function knowing what an endpoint is, and so a test can
/// drive it with scripted delays and no sockets at all.
pub(super) async fn race_candidates<C, F, Fut, T>(
    candidates: Vec<C>,
    stagger: Duration,
    mut attempt: F,
) -> RaceOutcome<C, T>
where
    C: Clone,
    F: FnMut(C) -> Fut,
    Fut: Future<Output = Option<T>>,
{
    async fn run_one<C, T>(
        candidate: C,
        attempt: impl Future<Output = Option<T>>,
    ) -> (C, Option<T>) {
        (candidate, attempt.await)
    }

    let mut queued: VecDeque<C> = candidates.into();
    let mut running = FuturesUnordered::new();
    let mut failed = Vec::new();

    let Some(first) = queued.pop_front() else {
        return RaceOutcome { won: None, failed };
    };
    running.push(run_one(first.clone(), attempt(first)));

    let mut next_start = if queued.is_empty() {
        None
    } else {
        Some(Box::pin(tokio::time::sleep(stagger)))
    };

    loop {
        tokio::select! {
            Some((candidate, outcome)) = running.next(), if !running.is_empty() => {
                if let Some(value) = outcome { return RaceOutcome { won: Some((candidate, value)), failed } }
                failed.push(candidate);
                if running.is_empty() && next_start.is_none() {
                    return RaceOutcome { won: None, failed };
                }
            }
            () = async {
                match next_start.as_mut() {
                    Some(sleep) => sleep.await,
                    None => std::future::pending::<()>().await,
                }
            }, if next_start.is_some() => {
                if let Some(candidate) = queued.pop_front() {
                    running.push(run_one(candidate.clone(), attempt(candidate)));
                }
                next_start = if queued.is_empty() {
                    None
                } else {
                    Some(Box::pin(tokio::time::sleep(stagger)))
                };
            }
        }
    }
}

/// [`race_candidates`] over `candidates`' own [`try_direct_endpoint`]
/// attempts, staggered by [`CONNECTION_ATTEMPT_DELAY`] and bounded as a whole
/// by `bound`, which the caller decides and which is NOT always the whole
/// borrow deadline: [`dial_peer_reaching_within`] hands the leading race only
/// [`LEADING_RACE_SHARE`] of it, precisely so a later step -- the dead-drop
/// retry, a punch -- still gets a turn when every leading candidate
/// blackholes.
///
/// The bound is on the race, not on any one attempt: a row with ten
/// endpoints must not outlive the caller's patience just because staggered
/// attempts made ten of them cheap to start. On a timeout nothing is known to
/// have failed as opposed to merely being cut off mid-attempt, so nothing is
/// reported failed: the caller cools down what [`RaceOutcome::failed`] names
/// and nothing else.
async fn run_direct_race(
    row: &PeerRow,
    candidates: Vec<Endpoint>,
    rendezvous: &[u16],
    per_attempt: Duration,
    bound: Duration,
) -> RaceOutcome<Endpoint, (SocketAddr, PeerStream)> {
    let race = race_candidates(
        candidates,
        CONNECTION_ATTEMPT_DELAY,
        |endpoint: Endpoint| async move {
            let addr = endpoint.direct_addr()?;
            try_direct_endpoint(row, addr, rendezvous, per_attempt).await
        },
    );
    match tokio::time::timeout(bound, race).await {
        Ok(outcome) => outcome,
        Err(_elapsed) => RaceOutcome {
            won: None,
            failed: Vec::new(),
        },
    }
}

/// Clear the process-local cooldown for exactly the locators a dead-drop
/// fetch just wrote, and no other.
///
/// A cooldown records that an address failed a moment ago, while a fresh
/// signed record from the peer itself is new evidence about where it is NOW,
/// and new evidence outranks a stale failure. Without this, [`dial_order`]'s
/// own cooldown filter hides a re-taught address for
/// [`crate::peer::reach::probe::PATH_RETRY_COOLDOWN`], the common case for a peer
/// whose router mapping moved and moved back, and the retry below would look
/// like it fetched nothing.
///
/// A named function and not an inline loop so a test can prove it clears
/// exactly `written`'s own locators and nothing else: `written` came from
/// [`crate::peer::reach::drop_exchange::fetch_for`], which is the one place that knows which
/// locators actually landed on the row, so a blanket clear here would be a
/// second, looser answer to a question that function already answered
/// precisely.
pub(super) fn clear_cooldowns_for_fetch(peer: PeerId, written: &[Endpoint]) {
    for endpoint in written {
        crate::peer::reach::probe::clear_cooldown(peer, endpoint.locator);
    }
}

/// How a dial got through: a socket this node opened, or a pinned Mac that
/// carried it.
///
/// One value rather than an `Option<SocketAddr>` plus a flag, because every
/// caller that re-dates an endpoint has to write a
/// [`crate::peer::config::Locator`] and the two variants ARE that enum's two
/// variants. A caller that only wants the stream ignores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reached {
    /// The socket that answered.
    Direct(SocketAddr),
    /// The pinned Mac that carried the stream to the peer.
    Via(PeerId),
    /// A hole punched through both routers, at the socket it landed on.
    ///
    /// Its own variant and not a [`Self::Direct`] because the address is a
    /// TRANSIENT mapping: the port is this slot's derivation and the router
    /// holding it stops within minutes, so writing it onto the row as an
    /// endpoint would put a dead socket at the front of the next dial's order.
    /// A caller that re-dates endpoints skips this one; the pair meets again
    /// by deriving the next slot's port, not by remembering this one.
    Punched(SocketAddr),
}

/// [`dial_peer_within`], with the peers file that makes a forwarded hop
/// possible, and so two more ways home:
///
/// 1. A [`crate::peer::config::Locator::Via`] endpoint on the row is FOLLOWED
///    rather than logged and skipped. Its position in the list is
///    [`dial_order`]'s and not this function's, so an operator who set
///    `paths.prefer: via` gets the hop tried first and one who did not gets it
///    tried last. That is the whole of the policy and it is stated once, there.
/// 2. When nothing on the row answered at all, every Mac this node holds
///    [`crate::peer::config::Allow::carry`] on is asked to carry, in
///    [`forwarders_for`]'s order. This is the case the row cannot describe: a
///    peer that moved and whose every recorded address is stale has no `Via`
///    endpoint to follow, because nothing in this build writes one, and a
///    mutual friend that IS reachable is the only way home left.
/// 3. If both of those still answered nothing and the dead drop is on for
///    this peer, this Mac fetches the friend's drop once and, on a write,
///    retries the direct candidates once more before a punch. See the
///    dead-drop block below for the two things that make that retry see the
///    address it just learned rather than nothing.
///
/// Reports WHICH path answered for the same reason [`dial_peer_with_endpoint`]
/// reports which socket did: a boxed stream cannot be asked afterwards, and a
/// caller that re-dates the row needs to write the locator that worked.
pub async fn dial_peer_reaching_within(
    row: &PeerRow,
    store: &PeerStore,
    borrow_timeout_ms: u64,
) -> Option<(Reached, PeerStream)> {
    let per_attempt = per_attempt_bound(borrow_timeout_ms);
    let deadline = Instant::now() + Duration::from_millis(borrow_timeout_ms.max(1));
    let rendezvous = crate::peer::reach::rendezvous_ports(&row.node, now_unix_seconds());
    let order = dial_order(row);

    // The leading contiguous run of `Locator::Direct` endpoints is raced
    // together; everything from the first non-direct endpoint onward is
    // untouched below. A direct endpoint can still occur later in `order`
    // (an unranked `Reverse` sorts in the same band as `Direct`, see
    // `probe::kind_rank`), which is exactly why this is a leading RUN and not
    // a filter: only the front of the list is raced, so a later direct
    // endpoint keeps trying sequentially in its own position rather than
    // jumping the queue ahead of whatever `probe` ranked above it.
    let leading_run = order
        .iter()
        .take_while(|endpoint| matches!(endpoint.locator, Locator::Direct { .. }))
        .count();
    let (leading, rest) = order.split_at(leading_run);

    if !leading.is_empty() {
        let leading_bound = Duration::from_millis((borrow_timeout_ms / LEADING_RACE_SHARE).max(1));
        let race = run_direct_race(
            row,
            leading.to_vec(),
            &rendezvous,
            per_attempt,
            leading_bound,
        )
        .await;
        for endpoint in race.failed {
            crate::peer::reach::probe::cool_down(row.node, endpoint.locator, crate::now_ms());
        }
        if let Some((endpoint, (addr, stream))) = race.won {
            crate::peer::reach::probe::clear_cooldown(row.node, endpoint.locator);
            return Some((Reached::Direct(addr), stream));
        }
    }

    for endpoint in rest {
        match endpoint.locator {
            Locator::Direct { addr } => {
                if let Some((reached, stream)) =
                    try_direct_endpoint(row, addr, &rendezvous, per_attempt).await
                {
                    crate::peer::reach::probe::clear_cooldown(row.node, endpoint.locator);
                    return Some((Reached::Direct(reached), stream));
                }
                crate::peer::reach::probe::cool_down(row.node, endpoint.locator, crate::now_ms());
            }
            // Both go out through another Mac and by the same call: a
            // `Reverse` endpoint names the friend holding a carrier for this
            // target, and asking that friend to forward is exactly what spends
            // the carrier (`tunnel::reach_target` takes it off the desk before
            // it tries to dial). The difference between the two is on the
            // FRIEND's side, not here.
            Locator::Via { node } | Locator::Reverse { node } => {
                if let Some(stream) =
                    forward_through(&node, &row.node, store, borrow_timeout_ms).await
                {
                    crate::peer::reach::probe::clear_cooldown(row.node, endpoint.locator);
                    return Some((Reached::Via(node), stream));
                }
                crate::peer::reach::probe::cool_down(row.node, endpoint.locator, crate::now_ms());
            }
        }
    }

    // The dead drop, only after every endpoint already on the row (direct or
    // `Via`), the leading race included, failed, and only for a peer this Mac
    // is allowed to trade addresses with. It is tried here, after the row's
    // own endpoints and before a punch or a third Mac's carried bytes are
    // spent, on the design's own flowchart.
    let dead_drop_file = store.file();
    if dead_drop_file.drop_is_live() && row.allow.control.drop {
        let now_s = now_unix_seconds();
        let drop_slot = crate::peer::config::drop::current_slot(now_s);
        if !crate::peer::reach::drop_exchange::fetched_this_slot(&row.node, drop_slot) {
            match dead_drop_file.open_drop_store() {
                Ok(drop_store) => {
                    match crate::peer::reach::drop_exchange::fetch_for(
                        &drop_store,
                        store.path(),
                        &row.node,
                        now_s,
                    )
                    .await
                    {
                        Ok(written) if !written.is_empty() => {
                            clear_cooldowns_for_fetch(row.node, &written);

                            // `PeerStore` caches the file it read and reloads it
                            // on the file's mtime; `fetch_for` just wrote that
                            // same file directly, and the write can land inside
                            // one filesystem tick, too fast for mtime to have
                            // moved. Read the row back off disk, bypassing the
                            // cache entirely, rather than trust a reload that
                            // may see nothing new.
                            match crate::peer::config::read_or_default(store.path()) {
                                Ok(fresh_file) => {
                                    if let Some(fresh) = fresh_file.row(&row.node) {
                                        // The addresses the drop just taught are
                                        // dialled as their own ranked band among
                                        // the direct candidates, not as a second
                                        // bespoke path: one race, over whichever
                                        // direct endpoints the row's own order
                                        // now returns. `EndpointSource::Drop` is
                                        // the weakest band in `source_rank`, so a
                                        // drop-learned address sorts behind
                                        // everything a handshake proved and
                                        // behind a brief, and a cooled address
                                        // this fetch did not just clear is
                                        // filtered out of the order exactly as it
                                        // was above.
                                        let retry_candidates: Vec<Endpoint> = dial_order(fresh)
                                            .into_iter()
                                            .filter(|endpoint| {
                                                matches!(endpoint.locator, Locator::Direct { .. })
                                            })
                                            .collect();
                                        if !retry_candidates.is_empty() {
                                            let race = run_direct_race(
                                                fresh,
                                                retry_candidates,
                                                &rendezvous,
                                                per_attempt,
                                                remaining(deadline),
                                            )
                                            .await;
                                            for endpoint in race.failed {
                                                crate::peer::reach::probe::cool_down(
                                                    fresh.node,
                                                    endpoint.locator,
                                                    crate::now_ms(),
                                                );
                                            }
                                            if let Some((endpoint, (addr, stream))) = race.won {
                                                crate::peer::reach::probe::clear_cooldown(
                                                    fresh.node,
                                                    endpoint.locator,
                                                );
                                                return Some((Reached::Direct(addr), stream));
                                            }
                                        }
                                    }
                                }
                                Err(err) => tracing::debug!(
                                    peer = %row.node.display(),
                                    error = %err,
                                    "peer serve: the peers file did not read back after a dead \
                                     drop write, so the retry tried no addresses"
                                ),
                            }
                        }
                        Ok(_) => {}
                        Err(err) => tracing::debug!(
                            peer = %row.node.display(),
                            error = %err,
                            "peer serve: dead drop fetch failed; the dial continues to a punch \
                             exactly as it does today"
                        ),
                    }
                }
                Err(refusal) => tracing::debug!(
                    peer = %row.node.display(),
                    error = %refusal,
                    "peer serve: dead drop store did not open; the dial continues as it does \
                     today"
                ),
            }
        }
    }

    // Step 2 of the dial order: a hole punched straight through both routers.
    // Before the carry below and after the row's own endpoints, because a
    // direct socket that works is the better answer and a carry spends a THIRD
    // machine's bytes. It is bounded by the same borrow timeout everything
    // else here is: a punch waits for a slot boundary, so a dial with ten
    // seconds of patience takes the slots that fit inside it and no others.
    let punch_budget_ms = u64::try_from(remaining(deadline).as_millis()).unwrap_or(u64::MAX);
    match punch_dial(row, store, punch_budget_ms).await {
        Ok((addr, stream)) => return Some((Reached::Punched(addr), stream)),
        Err(failure) => tracing::debug!(
            peer = %row.node.display(),
            failure = %failure,
            "peer serve: nothing on this peer's row answered and a punch did not get through either; a carried stream is what is left"
        ),
    }

    for forwarder in forwarders_for(&row.node, store) {
        if let Some(stream) = forward_through(&forwarder, &row.node, store, borrow_timeout_ms).await
        {
            tracing::info!(
                peer = %row.node.display(),
                via = %forwarder.display(),
                "peer serve: nothing on this peer's own row answered, and a Mac this node \
                 may ask to carry reached it"
            );
            return Some((Reached::Via(forwarder), stream));
        }
    }
    None
}

/// [`dial_peer_reaching_within`] for the callers that only want the stream, on
/// the operator's own borrow timeout.
pub async fn dial_peer_reaching(row: &PeerRow, store: &PeerStore) -> Option<PeerStream> {
    dial_peer_reaching_within(row, store, borrow_timeout_default_ms())
        .await
        .map(|(_, stream)| stream)
}

/// One attempt to reach `target` through the pinned Mac `forwarder`.
///
/// A failure is a debug line and a `None`: the caller has more candidates to
/// try and a forwarder that refuses is the ordinary answer, not an error. The
/// refusal TEXT is kept, because every one of `authorize_forward`'s refusals
/// names which grant was missing, and an operator debugging "why will this Mac
/// not carry for me" has nowhere else to read it.
async fn forward_through(
    forwarder: &PeerId,
    target: &PeerId,
    store: &PeerStore,
    borrow_timeout_ms: u64,
) -> Option<PeerStream> {
    let row = store.row(forwarder)?;
    let key = match NodeKey::load_or_mint(&node_key_dir(store)) {
        Ok(key) => key,
        Err(err) => {
            tracing::warn!(
                error = %err,
                "peer serve: this node has no keypair to open a forwarded stream with"
            );
            return None;
        }
    };
    // This node's own id is stamped into `via` so the forwarder refuses a
    // route that comes back here, which is `authorize_forward`'s cycle check
    // reading the one fact only the requester can supply.
    match open_forward_to(
        &row,
        key.secret_bytes(),
        target,
        &[key.id()],
        borrow_timeout_ms,
    )
    .await
    {
        Ok(stream) => Some(stream),
        Err(err) => {
            tracing::debug!(
                peer = %target.display(),
                via = %forwarder.display(),
                error = %err,
                "peer serve: that Mac did not carry a stream to this peer; trying the next way"
            );
            None
        }
    }
}

/// Every Mac this node may ask to CARRY a stream to `target`, in the order to
/// ask them.
///
/// # The grant is `carry` and it is the only admission
///
/// [`crate::peer::config::Allow::carry`] is "we may ask this Mac to carry us",
/// the direction its own doc-comment separates from `gateway` ("this peer may
/// ask US"). One operator act must not hand out both directions, so this reads
/// the one that means what it needs, exactly as
/// [`crate::peer::tunnel::egress::candidates_from`] does for the carry to an origin.
///
/// Three exclusions, each one a thing that cannot work rather than a policy:
/// the target itself (a Mac cannot carry a stream to itself, and
/// [`open_forward_to`] refuses it again), a candidate with
/// no DIRECT endpoint (reaching a forwarder through a forwarder is a chain
/// nothing here builds), and nothing else.
///
/// # The order is PROBE's, when PROBE has measured anything
///
/// [`crate::peer::reach::probe::carrier_key`] is the sort, and its third part is what
/// makes it safe: a carrier nobody has probed ties at `u32::MAX` and falls
/// through to most-recently-seen-first, which is the order this would have had
/// without a path table at all. `paths.viaAllow`, when the operator set one,
/// sorts a Mac that is not on it behind every Mac that is: behind, and still
/// tried, the same rule [`crate::peer::reach::probe::order_endpoints`] applies to a
/// `Via` endpoint: a path that is the only path is the way home.
pub fn forwarders_for(target: &PeerId, store: &PeerStore) -> Vec<PeerId> {
    let file = store.file();
    let last_seen = last_seen_by_peer(store);
    let mut chosen: Vec<&PeerRow> = file
        .carriers()
        .into_iter()
        .filter(|row| row.node != *target)
        .filter(|row| row.endpoints.iter().any(|end| end.direct_addr().is_some()))
        .collect();
    let order = |row: &PeerRow| {
        let seen = last_seen
            .iter()
            .find(|(peer, _)| *peer == row.node)
            .map(|(_, at)| *at);
        crate::peer::reach::probe::with_table(|table| table.carrier_rank(&row.node, seen))
            .unwrap_or_else(|_| {
                (
                    0,
                    (0, u32::MAX, std::cmp::Reverse(seen.unwrap_or(i64::MIN))),
                )
            })
    };
    chosen.sort_by_key(|row| order(row));
    chosen.into_iter().map(|row| row.node).collect()
}

/// Whether this row has ANY way back: its own endpoints, or a Mac this node may
/// ask to carry to it.
///
/// The question [`crate::peer::serve::fallback::FallbackProvider::try_serve`] asks
/// before it spends anything on a lender, spelled here beside the dial that
/// answers it for real: a filter that said "no endpoints, skip" would skip
/// exactly the peer a forwarder exists for.
pub fn has_a_way_back(row: &PeerRow, store: &PeerStore) -> bool {
    row.has_endpoint() || !forwarders_for(&row.node, store).is_empty()
}

/// When each pinned Mac was last heard from, out of the runtime state file
/// beside the peers file.
///
/// A missing or unreadable state file is an empty list and not a refusal: it
/// costs the candidates their ordering and nothing else.
fn last_seen_by_peer(store: &PeerStore) -> Vec<(PeerId, i64)> {
    let state_path = peer_state_path(store.path());
    match crate::peer::state::load(&state_path, crate::peer::state::now_ms()) {
        Ok(state) => state.last_seen,
        Err(err) => {
            tracing::debug!(
                error = %err,
                "peer serve: no runtime state to order the carriers by; asking them in file \
                 order"
            );
            Vec::new()
        }
    }
}

/// One connect attempt, bounded.
///
/// A timeout and a refusal are the same outcome to the caller, which is "try
/// the next one", and both are logged with the address so a reader can tell
/// which of the four attempts against one host it was.
async fn connect_within(addr: SocketAddr, within: Duration) -> Option<PeerStream> {
    // A tailnet address, from a process running tcr's own tailnet node, goes
    // through the node: this Mac has no `utun` for it, so the kernel cannot
    // route it. Every other dial, and every dial on a Mac with the Tailscale
    // app, goes through the kernel as before.
    if let Some(through_node) =
        crate::peer::reach::tailnet::node::dial_through_node(addr, within).await
    {
        return match through_node {
            Ok(stream) => Some(stream),
            Err(err) => {
                tracing::debug!(
                    addr = %addr,
                    error = %err,
                    "peer serve: that address did not answer through the tailnet node; trying \
                     the next one"
                );
                None
            }
        };
    }
    match tokio::time::timeout(within, tokio::net::TcpStream::connect(addr)).await {
        Ok(Ok(stream)) => Some(Box::new(stream) as PeerStream),
        Ok(Err(err)) => {
            tracing::debug!(
                addr = %addr,
                error = %err,
                "peer serve: that address did not answer; trying the next one"
            );
            None
        }
        Err(_elapsed) => {
            tracing::debug!(
                addr = %addr,
                timeout_ms = within.as_millis(),
                "peer serve: that address did not answer in time; trying the next one"
            );
            None
        }
    }
}

/// Seconds since the unix epoch, for the port slot.
fn now_unix_seconds() -> u64 {
    u64::try_from(crate::now_ms().max(0)).unwrap_or_default() / 1_000
}

/// [`dial_peer_with_endpoint`] for a caller that only wants the stream.
pub async fn dial_peer(row: &PeerRow) -> Option<PeerStream> {
    dial_peer_with_endpoint(row).await.map(|(_, stream)| stream)
}

/// Step 2 of the dial order: punch a hole straight through both routers.
///
/// # The order of the three things this does
///
/// The ask goes out FIRST and the punch only runs if it was carried. A punch
/// is two sides in one slot; one side alone is a connect into a router that
/// was never told to hold a port open, and it costs the slot's full width to
/// learn that. So a peer that could not be told is a named refusal here
/// ([`crate::peer::reach::PunchFailure::NotAnnounced`]) rather than thirty
/// seconds of waiting.
///
/// The ask itself travels as a CONTROL frame over a carried stream: a Mac
/// nobody can dial is, by definition, a Mac that cannot be told anything
/// directly, and a mutual friend carries the frame BLIND, as a fresh
/// end-to-end Noise session nested inside a TUNNEL.
async fn punch_dial(
    row: &PeerRow,
    store: &PeerStore,
    borrow_timeout_ms: u64,
) -> Result<(SocketAddr, PeerStream), crate::peer::reach::PunchFailure> {
    use crate::peer::reach::{self, PunchFailure};

    // **The one refusal worth a second question, and only from a row that
    // names some way to that peer.** `PeerAddressUnknown` is the only refusal
    // an exchange can answer: it means nothing on this Mac knows where that
    // peer is, which is precisely the state a hint fixes. Every other refusal
    // is about the pair, the slot or this Mac, so asking the peer anything
    // would spend a carried round trip to learn what is already known, and
    // each returns exactly as it did before this step existed.
    //
    // **A row with NO endpoint at all is the carry case and not a punch, so it
    // is not asked.** An empty row is what a lender that parked a socket at a
    // friend looks like from here, and the step below this one already answers
    // it by asking that friend to carry ([`dial_peer_reaching_within`]'s last
    // loop). Asking first spends the same friend twice: one more connection
    // from this address against its pre-authentication allowance
    // ([`crate::peer::listener::unauthenticated_allowance`]), and on the
    // reverse path one of the carriers parked on its desk, both of them the
    // borrow's own and both spent on a punch that a Mac nobody can dial cannot
    // answer. Measured, not reasoned: with the question asked on an empty row,
    // `an_undialable_lender_ends_the_borrow_through_a_carrier_and_never_hangs`
    // (`tests/peer/peer_e2e.rs`) fails on both platforms of one run with the carrier
    // refusing a connection at an allowance of two, and
    // `a_row_with_no_endpoints_is_reached_through_a_carry_grantee`
    // (`tests/peer/reach/peer_forward.rs`) only passes if its fixture accepts a carried
    // stream the borrow never gets to use.
    //
    // ONE exchange, never a loop. A second ask against a peer that did not
    // answer the first spends a slot's worth of the caller's budget for
    // nothing, and there is no third thing to learn from asking twice.
    let (peer_ip, secret) = match reach::punch_target(&row.node, &row.endpoints) {
        Ok(found) => found,
        Err(PunchFailure::PeerAddressUnknown) if row.has_endpoint() => {
            let learned = exchange_addresses(row, store, borrow_timeout_ms).await;
            tracing::debug!(
                peer = %row.node.display(),
                learned,
                "peer punch: nothing here knew where that peer is, so this node asked it \
                 through a friend before aiming"
            );
            reach::punch_target(&row.node, &row.endpoints)?
        }
        Err(refused) => return Err(refused),
    };
    let Some(mine) = reach::self_address_for(&row.node) else {
        return Err(PunchFailure::SelfAddressUnknown);
    };

    let now = crate::now_ms();
    let budget = Duration::from_millis(borrow_timeout_ms);
    let plan = reach::punch_slots_within(
        &reach::punch_plan(&secret, now, reach::PUNCH_SLOTS),
        now,
        budget,
    );
    let Some(first) = plan.first() else {
        // Named, and immediate: this is the ordinary case for a short borrow
        // timeout, and spending the caller's whole budget waiting for a
        // boundary it cannot reach would be the worst of both.
        return Err(PunchFailure::NoSlotConnected {
            slots_tried: 0,
            ports: Vec::new(),
        });
    };

    if !announce_punch(row, store, first.slot, mine, borrow_timeout_ms).await {
        return Err(PunchFailure::NotAnnounced);
    }

    let punched = reach::punch(
        &reach::KernelPunchNet,
        &plan,
        peer_ip,
        reach::slot_window().min(budget),
    )
    .await?;
    Ok((SocketAddr::new(peer_ip, punched.port), punched.stream))
}

/// Tell `row`'s Mac to be in slot `slot`, through any Mac that will carry the
/// frame. Answers whether one did.
///
/// A boolean and not an error: every refusal on the way is a forwarder that
/// said no, which is the ordinary answer and is already logged with its own
/// reason by [`forward_through`]. What the caller needs is whether the peer
/// was told, because that is what decides whether the slot is worth waiting
/// for.
async fn announce_punch(
    row: &PeerRow,
    store: &PeerStore,
    slot: u64,
    public_addr: SocketAddr,
    borrow_timeout_ms: u64,
) -> bool {
    let key = match NodeKey::load_or_mint(&node_key_dir(store)) {
        Ok(key) => key,
        Err(err) => {
            tracing::warn!(
                error = %err,
                "peer punch: this node has no keypair to ask for a punch with"
            );
            return false;
        }
    };

    let ask = Control::PunchAt {
        slot,
        public_addr: public_addr.to_string(),
    };
    match ask_through_a_friend(row, store, borrow_timeout_ms, &key, &ask).await {
        Some((carrier, _stream, _session)) => {
            tracing::info!(
                peer = %row.node.display(),
                via = %carrier.display(),
                slot,
                "peer punch: asked this peer to meet on the pair's derived port"
            );
            true
        }
        None => false,
    }
}

/// Ask `row`'s Mac one thing through any Mac that will carry the frame, and
/// hand the LIVE stream back so a caller may read an answer on it.
///
/// # Why this is one function and not two copies
///
/// Which carrier is tried, in which order, and what the inner header says are
/// each one decision. A second spelling of the loop would be a second place
/// that decides them, and the pair would disagree the first time either was
/// edited: the punch's ask and the address exchange's ask travel the identical
/// way and differ only in the frame.
///
/// # The signature, and why it hands three things back
///
/// The CARRIER's id, because every line either caller logs names the Mac that
/// carried. The stream and the session, because the address exchange reads one
/// bounded answer on this stream and a helper that closed it would make that
/// impossible; a caller with nothing to read simply drops both, which is what
/// [`announce_punch`] does and what it did before this was extracted.
///
/// The key comes IN rather than being loaded here, so each caller keeps its
/// own sentence for "this node has no keypair": a shared helper would have to
/// pick one of the two, and an operator reading the line needs to know which
/// ask it was that could not be made.
///
/// A refusal on the way is a `None`, never an error: every one of them is a
/// forwarder that said no, which is the ordinary answer and is already logged
/// with its own reason by [`forward_through`].
async fn ask_through_a_friend(
    row: &PeerRow,
    store: &PeerStore,
    borrow_timeout_ms: u64,
    key: &NodeKey,
    ask: &Control,
) -> Option<(PeerId, PeerStream, noise::PeerSession)> {
    for forwarder in forwarders_for(&row.node, store) {
        let Some(mut stream) =
            forward_through(&forwarder, &row.node, store, borrow_timeout_ms).await
        else {
            continue;
        };
        // The handshake runs against the TARGET's pinned key over whatever
        // stream came back, which is what makes the forwarder blind to this
        // frame exactly as it is to a SERVE.
        let session = noise::dial_handshake(
            &mut stream,
            key.secret_bytes(),
            Handshake::Return,
            Some(&row.node.0),
            None,
        )
        .await;
        let mut session = match session {
            Ok(session) => session,
            Err(err) => {
                tracing::debug!(
                    peer = %row.node.display(),
                    via = %forwarder.display(),
                    error = %err,
                    "peer carry: the carried handshake failed; trying another carrier"
                );
                continue;
            }
        };
        let request_id = match crate::peer::config::id::random_id() {
            Ok(id) => id,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "peer carry: this node could not mint a request id"
                );
                return None;
            }
        };
        // **The carrier goes in the inner `via`, and only this end can put it
        // there.** A blind forward writes no header of its own
        // (`crate::peer::tunnel`'s doc on that point), so the origin is the
        // only party that knows which Mac carried, and a receiver whose inner
        // `via` is empty reads the socket in front of it as the peer's own.
        // That socket is the carrier's connection and teaches nothing about
        // the peer, which is what `listener::StreamOrigin` exists to say.
        // Stamping it here is what lets the far end say it.
        let header = StreamHeader {
            kind: StreamKind::Control,
            target: None,
            via: vec![forwarder],
            hops_remaining: 1,
            request_id,
        };
        let asked = async {
            send_control(&mut stream, &mut session, &header).await?;
            send_control(&mut stream, &mut session, ask).await
        }
        .await;
        match asked {
            Ok(()) => return Some((forwarder, stream, session)),
            Err(err) => tracing::debug!(
                peer = %row.node.display(),
                via = %forwarder.display(),
                error = %err,
                "peer carry: the ask did not reach this peer through that carrier"
            ),
        }
    }
    None
}

/// How long the address exchange waits for the other Mac's hint back.
///
/// Bounded twice over, and both bounds matter. Never more than the caller's
/// own borrow timeout, because a dial that overran it would report a peer
/// unreachable after the operator's patience had already run out. And never
/// more than this, because the answer is one frame on a stream that is already
/// open and a peer that heard the hint but says nothing back would otherwise
/// eat the punch that follows: the exchange exists to make the punch possible,
/// so spending the punch's own waiting time on it is the one way it can make
/// things worse. Five seconds is a whole `Probe` round trip across a relay
/// with room to spare.
const HINT_ANSWER_TIMEOUT: Duration = Duration::from_secs(5);

/// Swap current addresses with `row`'s Mac through a friend they can both
/// still reach. Answers whether an address was learned.
///
/// # The moment this is for
///
/// Both Macs changed networks, so each one's recorded address for the other is
/// somewhere neither of them is any more. Neither can be told anything
/// directly and neither has anything to aim at. What they do still share is a
/// friend, so the hint goes out over the friend and the answer comes back on
/// the same stream: one round trip, and then a direct path between the two of
/// them that the friend carries nothing more of.
///
/// The row has to name SOME way to that peer for the question to be asked at
/// all, which is [`punch_dial`]'s own guard and is recorded there: a row with
/// nothing on it is a peer the dial reaches by asking a friend to carry, and
/// spending that friend on a hint first is what takes the borrow's own carrier
/// away.
///
/// # One exchange, one answer read
///
/// Exactly one bounded [`recv_control`], on [`HINT_ANSWER_TIMEOUT`]. A peer
/// running a build with no arm for this frame closes the session instead of
/// answering, which reads here as "this peer does not exchange addresses" and
/// is the same thing the prober already does with an unanswered `Probe`: the
/// caller falls through to the carry, which still works.
///
/// An answer naming a Mac other than the one asked is refused rather than
/// believed. A hint is about its own sender, and a third party's address
/// travels as a `NeighborBrief` under a grant
/// ([`crate::peer::discovery::neighbor_brief_endpoints`]); believing one here
/// would be transitive trust with nothing behind it, and the refusal is the
/// same rule the listener's own arm applies in the other direction.
async fn exchange_addresses(row: &PeerRow, store: &PeerStore, borrow_timeout_ms: u64) -> bool {
    let key = match NodeKey::load_or_mint(&node_key_dir(store)) {
        Ok(key) => key,
        Err(err) => {
            tracing::warn!(
                error = %err,
                "peer aim: this node has no keypair to exchange addresses with"
            );
            return false;
        }
    };

    let ask = Control::CollapseHint(crate::peer::reach::own_collapse_hint(key.id(), &row.node));
    let Some((carrier, mut stream, mut session)) =
        ask_through_a_friend(row, store, borrow_timeout_ms, &key, &ask).await
    else {
        tracing::debug!(
            peer = %row.node.display(),
            "peer aim: no pinned Mac could carry this node's address to that peer, so there \
             is nothing to exchange"
        );
        return false;
    };

    let within = HINT_ANSWER_TIMEOUT.min(Duration::from_millis(borrow_timeout_ms));
    let answered = match tokio::time::timeout(
        within,
        recv_control::<_, Control>(&mut stream, &mut session),
    )
    .await
    {
        Ok(Ok(frame)) => frame,
        Ok(Err(err)) => {
            tracing::debug!(
                peer = %row.node.display(),
                via = %carrier.display(),
                error = %err,
                "peer aim: that peer closed the carried session instead of answering with its \
                 own address, so this build reads it as a peer that does not exchange \
                 addresses"
            );
            return false;
        }
        Err(_elapsed) => {
            tracing::debug!(
                peer = %row.node.display(),
                via = %carrier.display(),
                timeout_ms = within.as_millis(),
                "peer aim: that peer sent no address back inside the deadline"
            );
            return false;
        }
    };

    let Control::CollapseHint(theirs) = answered else {
        tracing::debug!(
            peer = %row.node.display(),
            via = %carrier.display(),
            "peer aim: that peer answered an address exchange with another frame"
        );
        return false;
    };
    if theirs.node != row.node {
        tracing::warn!(
            peer = %row.node.display(),
            via = %carrier.display(),
            named = %theirs.node.display(),
            "peer aim: that peer answered with a hint about a third Mac, which is a \
             NeighborBrief's job and needs a grant, so nothing was recorded"
        );
        return false;
    }
    if let Some(addr) = crate::peer::reach::learn_from_hint(&row.node, &theirs) {
        tracing::info!(
            peer = %row.node.display(),
            via = %carrier.display(),
            addr = %addr,
            "peer aim: that peer named where it is now, over a friend they both still \
             reach"
        );
        true
    } else {
        tracing::debug!(
            peer = %row.node.display(),
            via = %carrier.display(),
            "peer aim: nothing that peer named is an address anything can dial, so this \
             node still has nowhere to aim"
        );
        false
    }
}

// ---------------------------------------------------------------------------
// The requester's half
// ---------------------------------------------------------------------------

/// How many hops a client asks a forwarder for.
///
/// One. The requester spends it at the forwarder and the target is the far
/// end, so a second hop is a chain nothing in this build constructs, and a
/// budget that is not spent is a budget an intermediate could spend on this
/// node's behalf. [`authorize_forward`] clamps it again by the FORWARDER's own
/// `maxHops`, so this number can only ever ask for less than the operator
/// there allows.
///
/// [`authorize_forward`]: super::authorize_forward
pub const CLIENT_FORWARD_HOPS: u8 = 1;

/// Ask `forwarder` to carry a stream to `target`, and return that stream.
///
/// # This is the half that did not exist, and what its absence meant
///
/// Everything else on the forwarded path was built and gated
/// ([`authorize_forward`], [`handle_forward_on`]) and nothing could reach it:
/// no code anywhere opened a `StreamKind::Tunnel` whose target was a peer, so
/// a row whose only endpoint was a [`crate::peer::config::Locator::Via`] was
/// dialled by nobody and `serve.rs`'s dial loop logged it and moved on. This
/// function is that opening, and [`dial_peer_reaching`] is
/// the caller that decides when to use it.
///
/// # What comes back, and why the caller cannot tell it apart
///
/// A [`crate::peer::noise::PeerStream`], exactly what a direct dial returns.
/// The forwarder splices the onward socket RAW ([`handle_forward_on`]), so
/// the bytes this stream carries are the target's own socket, and the caller
/// runs its ordinary handshake over it against the TARGET's pinned key. That
/// is the whole of the blindness argument from this side: this node holds a
/// session with the forwarder that carries a second session the forwarder has
/// no key for.
///
/// # Ordering, which is not an accident
///
/// The header is written before this returns, so the forwarder is already
/// waiting on the requester's first bytes ([`handle_forward_on`]'s deadline)
/// when the caller starts its handshake with the target. A caller that wrote
/// nothing would be closed by that deadline rather than pinning a task on the
/// forwarder, which is why nothing here waits for an acknowledgement: a
/// forward has none to give: the next thing the forwarder does is dial, and
/// its refusals all happen before any byte of the nested session moves.
///
/// The dial to the forwarder is [`dial_peer_within`],
/// which follows DIRECT endpoints only. That is what bounds this: a forwarder
/// reached through a forwarder would be a chain this node builds one hop at a
/// time with no budget of its own, and the hop count is the forwarder's bound,
/// never the requester's.
///
/// [`authorize_forward`]: super::authorize_forward
/// [`handle_forward_on`]: super::handle_forward_on
pub async fn open_forward_to(
    forwarder: &PeerRow,
    node_secret: &[u8; crate::peer::config::id::KEY_BYTES],
    target: &PeerId,
    via: &[PeerId],
    borrow_timeout_ms: u64,
) -> Result<PeerStream> {
    if forwarder.node == *target {
        bail!(
            "peer forward: {} cannot carry a stream to itself; a direct dial is what that \
             asks for",
            target.display()
        );
    }
    let (_reached, mut stream) = dial_peer_within(forwarder, borrow_timeout_ms)
        .await
        .ok_or_else(|| {
            anyhow!(
                "peer forward: none of {}'s own addresses answered, so it cannot carry \
                     anything to {}",
                forwarder.label,
                target.display()
            )
        })?;
    let mut session = noise::dial_handshake(
        &mut stream,
        node_secret,
        Handshake::Return,
        Some(&forwarder.node.0),
        None,
    )
    .await
    .context("peer forward: the handshake with the forwarder failed")?;

    let header = StreamHeader {
        kind: StreamKind::Tunnel,
        target: Some(TunnelTarget::Peer { node: *target }),
        via: via.to_vec(),
        hops_remaining: CLIENT_FORWARD_HOPS,
        request_id: crate::peer::config::id::random_id()?,
    };
    let bytes = serde_json::to_vec(&header)
        .context("peer forward: the forwarding header did not serialize")?;
    noise::send_encrypted(&mut stream, &mut session.transport, &bytes)
        .await
        .context("peer forward: the forwarder did not take the stream header")?;

    tracing::debug!(
        forwarder = %forwarder.node.display(),
        target = %target.display(),
        "peer forward: asked a pinned Mac to carry a stream to another pinned Mac"
    );
    Ok(Box::new(NoiseStream::start(stream, session)))
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::time::Instant;

    use tcr_peer_wire::PeerId;

    use super::*;
    use crate::peer::config::{Endpoint, Locator, PeerRow};

    /// **The stagger, through the private race itself, with no sockets at
    /// all.**
    ///
    /// Two scripted candidates: the first takes 600 ms to answer, the second
    /// only 5 ms. The second is started [`CONNECTION_ATTEMPT_DELAY`] after
    /// the first, so at 255 ms it is already ahead of the first candidate's
    /// own 600 ms clock, and must win. A stagger short enough to let the
    /// slow candidate finish before the fast one even starts (anything at or
    /// above 595 ms) would prove nothing but the order of two futures; 600 ms
    /// leaves headroom above the real 250 ms delay while still failing loudly
    /// if the delay were ever raised past it.
    ///
    /// Watched red: raise [`CONNECTION_ATTEMPT_DELAY`] to 10 seconds. The
    /// second candidate then starts long after the first's own 600 ms clock
    /// has already answered, so the first (slow) candidate wins instead:
    ///
    /// ```text
    /// assertion `left == right` failed: the second candidate started
    /// CONNECTION_ATTEMPT_DELAY after the first and still answers first: it
    /// must win
    ///   left: Some(600ms)
    ///  right: Some(5ms)
    /// ```
    #[tokio::test]
    async fn the_race_starts_its_next_candidate_a_stagger_after_the_previous_one() {
        let slow = Duration::from_millis(600);
        let fast = Duration::from_millis(5);
        let candidates = vec![slow, fast];

        let started = Instant::now();
        let outcome = race_candidates(
            candidates,
            CONNECTION_ATTEMPT_DELAY,
            |delay: Duration| async move {
                tokio::time::sleep(delay).await;
                Some(delay)
            },
        )
        .await;
        let elapsed = started.elapsed();

        assert_eq!(
            outcome.won.map(|(_, value)| value),
            Some(fast),
            "the second candidate started CONNECTION_ATTEMPT_DELAY after the first and still \
             answers first: it must win"
        );
        assert!(
            outcome.failed.is_empty(),
            "the slow candidate was still running when the fast one won; it was cancelled, \
             not refused, so it must not be in the failed list: {:?}",
            outcome.failed
        );
        assert!(
            elapsed < slow,
            "the race must not wait for the cancelled candidate's own clock: took {elapsed:?}"
        );
    }

    /// **`clear_cooldowns_for_fetch` clears exactly the locators it is
    /// handed, and no other.**
    ///
    /// Two locators on one peer, both cooling. Only one is passed as
    /// `written`. Read back through [`crate::peer::reach::probe::dial_order`], the
    /// same public function the dead-drop retry calls: if the cleared
    /// locator alone comes back, the clear was scoped; if the untouched
    /// locator ALSO comes back, either nothing was cleared (so
    /// `dial_order`'s own "every path is cooling, try them all anyway"
    /// fallback fired) or the clear was a blanket one.
    ///
    /// Watched red: comment out the `clear_cooldown` call inside
    /// `clear_cooldowns_for_fetch`. The untouched locator's cooldown then
    /// never lifts, both locators stay cooling, `dial_order`'s fallback
    /// returns BOTH, and this test's `assert!(!order.iter().any(...))` for
    /// the untouched locator fails: `left: true` where the assertion
    /// expects it absent.
    #[test]
    fn clear_cooldowns_for_fetch_only_clears_the_locators_it_is_given() {
        let peer = PeerId([0x60; 32]);
        let cleared_addr: SocketAddr = "127.0.0.1:1".parse().expect("a socket address");
        let untouched_addr: SocketAddr = "127.0.0.1:2".parse().expect("a socket address");
        let cleared_locator = Locator::Direct { addr: cleared_addr };
        let untouched_locator = Locator::Direct {
            addr: untouched_addr,
        };

        let until_ms = crate::now_ms() + 60_000;
        crate::peer::reach::probe::with_table(|table| {
            table.cool_down(peer, cleared_locator, until_ms);
            table.cool_down(peer, untouched_locator, until_ms);
        })
        .expect("the process-local path table is not poisoned");

        let written = vec![Endpoint::direct(
            cleared_addr,
            crate::now_ms(),
            crate::peer::config::EndpointSource::Drop,
        )];
        clear_cooldowns_for_fetch(peer, &written);

        let mut row = PeerRow {
            node: peer,
            label: "clear-cooldowns-for-fetch-test".to_string(),
            endpoints: Vec::new(),
            added_at: 0,
            allow: crate::peer::config::Allow::default(),
            lend: Vec::new(),
            rendezvous_secret: None,
            sees_us_at: None,
        };
        row.observe_endpoint(Endpoint::direct(
            cleared_addr,
            1_000,
            crate::peer::config::EndpointSource::Drop,
        ));
        row.observe_endpoint(Endpoint::direct(
            untouched_addr,
            1_000,
            crate::peer::config::EndpointSource::Drop,
        ));

        let order = dial_order(&row);
        assert!(
            order
                .iter()
                .any(|endpoint| endpoint.locator == cleared_locator),
            "the locator the fetch wrote must be dialable again: {order:?}"
        );
        assert!(
            !order
                .iter()
                .any(|endpoint| endpoint.locator == untouched_locator),
            "a locator the fetch did NOT write must stay excluded, not swept in by a blanket \
             clear or by dial_order's own cooling-fallback: {order:?}"
        );
    }
}
