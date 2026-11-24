use std::net::SocketAddr;
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use tcr_peer_wire::{LeaseRefusal, PROTO_VERSION, PeerId, Window};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::peer::config::PeerStore;
use crate::peer::lease::Ledger;
use crate::peer::noise;

use super::frame::{
    MAX_RELAYED_BODY_BYTES, SERVE_FLOW, ServeAck, ServeReply, ServeRequest, recv_body, recv_json,
    send_body, send_json,
};
use super::policy::{
    SERVE_METHOD, lender_forwards_header, refused_path_kind, serve_is_allowed_for_path,
};

/// The message of the lender's one log line per served request.
///
/// A constant rather than a literal in the `info!` because a gate greps for it
/// ([`handle_serve_on`] emits it, `tests/peer_boot.rs` asserts its field set),
/// and a gate keyed on a copy of a message is a gate that goes quiet the day
/// somebody rewords the message. The FIELDS are the contract as much as the
/// text: `peer`, `window`, `bytes`, `ms`, and nothing else.
pub const SERVED_LINE: &str = "peer serve: served a relayed request on this Mac's own account";

/// One account's utilization on one window, read from the lender's own fleet.
///
/// A trait rather than a `Manager` handle in the signature so the lender's half
/// can be driven without a fleet: the production implementation is
/// [`crate::manager::Manager`] and reads its accounts' quota, and a test hands
/// in a reader whose before/after answers are chosen.
///
/// The answer is **per account, positionally stable**: `select_with_group`
/// returns an index into a vector accounts are appended to and never removed
/// from, so index `i` is the same account across the two reads that bracket one
/// relayed request. A single scalar cannot carry the same fact, the fleet's
/// maximum can move because a DIFFERENT account's window moved, which is why
/// [`utilization_rise`] compares elementwise.
///
/// `None` at an index is "never measured", never "empty": see
/// [`utilization_rise`] for what that costs the lease.
pub trait WindowUtilization: Send + Sync {
    /// Every account's utilization on `window`, in account order.
    fn read(&self, window: Window) -> Vec<Option<f64>>;

    /// How a relayed request can be held inside `scope` on THIS node's fleet,
    /// which is the picker restriction.
    ///
    /// On the same trait as the utilization read because both are questions
    /// about the lender's own fleet that the serving leg has to ask without
    /// holding a `Manager` (see this trait's doc). The production answer is
    /// [`crate::manager::Manager`]'s, which reads its groups and its spill
    /// settings.
    ///
    /// **The default answers [`ScopeRestriction::Unenforceable`] for anything
    /// but [`tcr_peer_wire::LendScope::All`]**, and that direction is the
    /// decision: a reader that cannot resolve the lender's groups or its
    /// account labels cannot prove a request stayed inside the scope, and an
    /// unprovable restriction on somebody else's account is the failure this
    /// whole file exists to prevent. `handle_serve_on` refuses rather than
    /// serving out of scope.
    fn scope_restriction(&self, scope: &tcr_peer_wire::LendScope) -> ScopeRestriction {
        match scope {
            tcr_peer_wire::LendScope::All => ScopeRestriction::Unrestricted,
            tcr_peer_wire::LendScope::Group(_) | tcr_peer_wire::LendScope::Accounts(_) => {
                ScopeRestriction::Unenforceable
            }
        }
    }

    /// How much of `window` this node's guard band leaves **on `scope`'s
    /// accounts only**, or `None` for a reader that cannot answer per scope.
    ///
    /// The rule is: "the fraction is of the SCOPE's headroom (one account's
    /// window, the set's pooled window, the group's pooled window), computed by
    /// `lendable_fraction` over the scope's accounts only." A measurement
    /// found `Ledger::note_owner_headroom` with no production caller and the
    /// ledger's headroom keyed by window alone, so a group-scoped lease was
    /// clamped by (and its owner guard decided against) the whole fleet.
    ///
    /// On this trait, and not on a `Manager` handle, for the reason the trait
    /// exists: the serving leg asks it without holding a fleet, and a test hands
    /// in a reader whose answers are chosen. The production implementation is
    /// [`crate::manager::Manager::lendable_fraction`], which is the SAME
    /// function `lent_to` and the picker restriction already agree with about
    /// which accounts a scope draws from.
    ///
    /// **`None` is not zero.** A reader that cannot answer leaves the ledger's
    /// last `LendScope::All` note in place as the ceiling
    /// (`Ledger::headroom_for`), which is what every pre-decision-12 lease
    /// already meant; answering `0.0` here would refuse every lease a test
    /// reader is behind.
    fn lendable(&self, _scope: &tcr_peer_wire::LendScope, _window: Window) -> Option<f64> {
        None
    }

    /// The same figure for a `hand` grant: what may be lent out of the accounts
    /// whose BEARER can leave this Mac, which is a different set from the one
    /// above and often a smaller one.
    ///
    /// A hand lease is spent on the borrower's Mac with a token this one hands
    /// over, so a figure measured on an account that cannot be handed over
    /// funds a lease no bearer backs. `Ledger::grant` clamps a hand grant by
    /// this as well, which is what makes "funded only if a bearer exists" hold.
    ///
    /// **`None` is "this reader cannot answer" and leaves the clamp off**, the
    /// same direction [`Self::lendable`] takes and for the same reason: a
    /// reader with no fleet behind it (every test one, and
    /// [`NoFleetUtilization`]) would otherwise refuse every hand lease. The
    /// production answer is `Manager`'s and is never `None`.
    fn lendable_by_hand(&self, _scope: &tcr_peer_wire::LendScope, _window: Window) -> Option<f64> {
        None
    }
}

/// How the lender's own proxy can be made to pick inside a lease's scope.
///
/// # Why this is three answers and not a list of accounts
///
/// The relayed request re-enters the lender's OWN proxy over loopback
/// ([`handle_serve_on`]), so the only thing that crosses into the picker is a
/// header. The proxy reads two account-selection headers and both are STRICT:
/// [`crate::proxy::GROUP_HEADER_NAME`], which `Manager::select_with_group`
/// holds inside the named group (`strict_group`, `src/manager/select.rs:1139`,
/// for every group that is reserved or not a spill group), and
/// [`crate::proxy::ACCOUNTS_HEADER_NAME`], which holds the request inside a
/// named SET of accounts by benching every account outside it before the
/// rotation loop starts.
///
/// [`Self::Unenforceable`] is what is left: a scope this build cannot prove a
/// request stayed inside, a spill-and-not-reserved group, a group with no
/// member, an account set naming nothing this fleet carries. It is a refusal
/// rather than a best-effort, because the alternative is serving a scoped
/// lease on whichever account the picker liked, which is exactly the thing the
/// operator scoped it to avoid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeRestriction {
    /// Every account may serve it, [`tcr_peer_wire::LendScope::All`].
    Unrestricted,
    /// Send it with this group on [`crate::proxy::GROUP_HEADER_NAME`], which
    /// the picker enforces strictly.
    Group(String),
    /// Send it with these account labels on
    /// [`crate::proxy::ACCOUNTS_HEADER_NAME`], which the picker enforces
    /// strictly. Already narrowed to the labels the lender's own fleet
    /// carries, so an empty list never reaches here, a set that named no
    /// local account is [`Self::Unenforceable`] instead, exactly as an empty
    /// group is.
    Accounts(Vec<String>),
    /// This build cannot hold the request inside the scope, so it must not
    /// serve it at all.
    Unenforceable,
}

/// The utilization rise one relayed request caused, as the largest rise any
/// single account showed between the two reads.
///
/// The largest single rise rather than the sum: one request is served by ONE
/// account (the lender's own picker chose it), so a sum over a fleet whose
/// other accounts were probed in the same interval would charge the lease for
/// the lender's own traffic.
///
/// **An index that was unmeasured before contributes nothing**, so the first
/// borrowed request against a never-probed fleet charges
/// [`crate::peer::lease::MIN_DEBIT`] rather than the whole of the first
/// measurement. That is the honest answer: a rise is a difference between two
/// measurements and there is only one here. The other direction would charge a
/// borrower for everything the owner had already spent.
///
/// A negative difference, a window that reset under the request, which these
/// headers really do report, is not a credit: it contributes `0.0` and
/// `Ledger::debit` then charges `MIN_DEBIT`.
pub fn utilization_rise(before: &[Option<f64>], after: &[Option<f64>]) -> f64 {
    before
        .iter()
        .zip(after.iter())
        .filter_map(|(before, after)| match (before, after) {
            (Some(before), Some(after)) => Some((after - before).max(0.0)),
            _ => None,
        })
        .fold(0.0_f64, f64::max)
}

/// A reader for a node that has no fleet to measure, every window unmeasured.
///
/// Named rather than an empty `Vec` at each call site: "this node cannot
/// measure its own utilization" is a fact a reader of the boot path should see
/// spelled out, and it is the honest state of a node that lends nothing.
pub struct NoFleetUtilization;

impl WindowUtilization for NoFleetUtilization {
    fn read(&self, _window: Window) -> Vec<Option<f64>> {
        Vec::new()
    }
}

/// The lender's half: serve one relayed request on THIS node's own account.
///
/// Runs this node's OWN picker, attaches its OWN Bearer, pays its OWN per-org
/// throttle bucket, and debits the lease from the utilization rise it observes.
///
/// **This improves the one-sender-per-org property rather than degrading it**:
/// the lender is the only sender on its own account, through the one bucket
/// that already exists and is already counted, so a borrowed request is ADDED
/// to it. (A blind tunnel is the asymmetric case, it gives an org a sender it
/// did not have, from another address. That is the feature working as designed,
/// and it is why a gateway's caps are per-peer byte caps.)
///
/// Keeps the borrower's path refusal as a backstop. It is not the enforcement
/// point.
///
/// # This signature cannot serve a request, and that is reported rather than
/// worked around
///
/// A lender's half needs the stream the request arrived on, the session that
/// authenticated it, the ledger that funds it and somewhere to send it. This
/// signature has the peer and the peers file, so what it can answer is the
/// lender's ADMISSION question, may this peer open a SERVE here at all, which
/// is a real check with a real answer and is the first thing
/// [`handle_serve_on`] asks. [`handle_serve_on`] is the whole of the lender's
/// half, the way
/// `pair::mint_invite_as` is the whole of an invite the skeleton's `mint_invite`
/// could not build either.
async fn handle_serve(peer: &PeerId, store: &PeerStore) -> Result<()> {
    store.reload_if_changed();
    let Some(row) = store.row(peer) else {
        bail!(
            "peer serve: {} is not pinned here, so it may not open a SERVE",
            peer.display()
        );
    };
    if !row.allow.inspect {
        bail!(
            "peer serve: {} has not been granted `inspect` here, so SERVE is not on offer \
             at all (`tcr peer allow <peer> inspect on`)",
            peer.display()
        );
    }
    Ok(())
}

/// The lender's half, on the stream the request arrived on.
///
/// `upstream` is where the relayed request is sent, and it is **this node's own
/// proxy address**. That is the whole of "own picker, own Bearer, own bucket":
/// the request re-enters the same handler every local client uses, so account
/// selection, the pooled Bearer substitution and the per-organization GCRA
/// (`Manager::throttle_send`, keyed by `Manager::throttle_bucket_key` off the
/// SERVING account's org) all happen exactly once, in the one place they are
/// already written and already tested. There is no second picker and no second
/// bucket to keep in step.
///
/// Reads one [`ServeRequest`] plus its body frame, answers one [`ServeReply`]
/// plus its body frame, and returns. One request per stream: a stream that
/// carries two has to answer what happens to the second when the first is
/// refused, and the answer nobody has to reason about is that there is no
/// second.
///
/// # `utilization` is the measured debit, and it is why the signature grew
///
/// The skeleton debited through `Ledger::debit(.., 0.0)`, always
/// [`crate::peer::lease::MIN_DEBIT`], a constant whose own doc calls itself a
/// guess, because the delta needs the serving account's utilization before and
/// after and this signature held no way to read one. It holds one now: the
/// window is read through [`WindowUtilization`] on either side of the serve and
/// [`utilization_rise`] turns the pair into the figure the lease is charged.
/// The production reader is the lender's own `Manager`; nothing else on the
/// manager is reached from here.
///
/// # `header_request_id` is the id the STREAM was opened under
///
/// [`ServeRequest::request_id`] is documented as "the same id as the
/// [`StreamHeader::request_id`] that opened the stream" and the lender never
/// compared them, which the review names as an edge of H2: the stream gate's
/// dedup admits the HEADER's id, so a fresh header id carrying a repeated frame
/// id walked past it. They are compared here, where both are in scope, and a
/// mismatch is refused rather than reconciled, there is no honest way to pick
/// which of two ids a request is.
pub async fn handle_serve_on<S>(
    stream: &mut S,
    session: &mut noise::PeerSession,
    store: &PeerStore,
    serving: &crate::peer::listener::LeaseServing,
    header_request_id: u128,
    now_ms: i64,
    from: SocketAddr,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // The three things a lender's half cannot derive, as the ONE struct the
    // listener already assembles them into. They were three parameters and the
    // request id made eight, which is one over clippy's bound, and an
    // `#[allow]` on a signature whose arguments really are a struct would be
    // silencing the lint rather than answering it.
    let ledger: &std::sync::Mutex<Ledger> = &serving.ledger;
    let upstream: &str = &serving.upstream;
    let utilization: &dyn WindowUtilization = serving.utilization.as_ref();
    handle_serve(&session.peer, store).await?;

    let request: ServeRequest = recv_json(stream, session).await?;

    if request.proto != PROTO_VERSION {
        bail!(
            "peer serve: this frame says wire version {} and this build speaks {}",
            request.proto,
            PROTO_VERSION
        );
    }
    // THE FLOW, REFUSED BY NAME AND NEVER GUESSED AT. A borrower older than
    // [`SERVE_FLOW`] writes its body immediately after this frame and waits for
    // a reply, so serving it would mean reading a body nobody acknowledged and
    // leaving that borrower to call every outcome "may have been billed". The
    // missing field reads 0, which is not a flow, and both numbers are in the
    // line so an operator knows which Mac to upgrade.
    if request.flow != SERVE_FLOW {
        bail!(
            "peer serve: {} speaks SERVE flow {} and this build speaks {SERVE_FLOW}; refused \
             rather than served, because the two disagree about when the body is written \
             (upgrade tcr on that Mac)",
            session.peer.display(),
            request.flow
        );
    }
    // THE BACKSTOP, not the enforcement point: the borrower refuses these paths
    // before it opens anything, which is the check that matters because a
    // lender-side refusal happens after the bytes are already in this process.
    // Kept because an older or a malicious borrower is exactly the case a
    // backstop is for.
    if !serve_is_allowed_for_path(&request.path) {
        bail!(
            "peer serve: {} refused for path {}, a relayed request may never carry {}",
            session.peer.display(),
            request.path,
            refused_path_kind(&request.path)
        );
    }
    // The method is HONOURED, which for this build means exactly one method is
    // servable and every other one is refused here rather than rewritten into a
    // POST further down. A borrower whose client sent `GET` and whose request
    // was silently POSTed on someone else's account got an answer to a question
    // it never asked, paid for out of a lease.
    if !request.method.eq_ignore_ascii_case(SERVE_METHOD) {
        bail!(
            "peer serve: {} sent method {} and a relayed request is {SERVE_METHOD} or nothing; \
             refused rather than rewritten",
            session.peer.display(),
            request.method
        );
    }

    // The two ids must be ONE id. See this function's doc.
    if request.request_id != header_request_id {
        bail!(
            "peer serve: {} opened the stream under request id {:032x} and sent a frame for \
             {:032x}; a relayed request is one id and this one is refused rather than \
             reconciled",
            session.peer.display(),
            header_request_id,
            request.request_id
        );
    }

    // THE LEASE'S SCOPE, read first: it is what decides which of this Mac's
    // accounts may serve the request AND which accounts' headroom the owner
    // guard is measured against, so both questions below need it in hand.
    let (window, scope) = {
        let held = ledger.lock().map_err(|_| anyhow!("ledger lock poisoned"))?;
        (
            held.window_of(request.lease_id),
            held.scope_of(request.lease_id),
        )
    };
    // THE LENDING HOURS, RE-READ PER RELAY and not only at the mint.
    //
    // The `--between` / `--days` schedule was consulted once, by `Ledger::grant`,
    // and never again. A lease minted a minute before the window closed then
    // kept serving on the owner's account until its own TTL or its `until`
    // said otherwise, which for a `--for 7d` grant is days outside the hours
    // the operator lent. An operator who writes "22:00-08:00" means the
    // requests, not the paperwork.
    //
    // Here rather than in `Ledger::may_relay`: the schedule lives on the
    // operator's own `LendGrant` in the peers file and the ledger holds no copy
    // of it, so this is the first point on the serving path that can read the
    // grant at all. Read fresh every relay, which is also what makes an
    // operator's edit to the hours take effect on the next request rather than
    // on the next mint.
    //
    // Before `enter_relay`, so a refusal takes no in-flight slot, and answered
    // with the ack frame: nothing has been sent upstream, so the borrower still
    // holds its body and may ask the next lender.
    if let Some(window) = window {
        let outside = store
            .row(&session.peer)
            .and_then(|row| {
                row.grant_for(
                    window,
                    u64::try_from(now_ms / 1_000).unwrap_or(0),
                    &|scope| {
                        utilization.scope_restriction(scope) != ScopeRestriction::Unenforceable
                    },
                )
                .and_then(crate::peer::config::LendGrant::schedule)
            })
            .and_then(|schedule| {
                crate::peer::lease::schedule_refusal(
                    Some(&schedule),
                    time::OffsetDateTime::now_utc(),
                )
            });
        if let Some(refusal) = outside {
            tracing::info!(
                peer = %session.peer.display(),
                window = ?window,
                "peer serve: this relay arrived outside the hours its grant was lent for, so \
                 it is refused rather than served"
            );
            send_json(stream, session, &ServeAck::Refused { refusal }).await?;
            return Ok(());
        }
    }

    // The owner's guard on THIS SCOPE's accounts, measured now, before
    // `enter_relay` reads it. A measurement found nothing in production wrote a
    // per-scope figure, so `may_relay` decided a group-scoped relay against the
    // whole fleet's room. Read outside the ledger lock, the reader reaches the
    // manager's accounts, and holding both is how a later edit deadlocks the
    // two.
    let scoped_headroom = window.and_then(|window| utilization.lendable(&scope, window));
    let entered = {
        let mut held = ledger.lock().map_err(|_| anyhow!("ledger lock poisoned"))?;
        // Every scope, `All` included. See `Ledger::grant` for why a reader
        // holding the fleet at the moment of the decision beats a note up to
        // thirty seconds old, and why `None` leaves the ticker's figure alone.
        if let (Some(window), Some(measured)) = (window, scoped_headroom) {
            held.note_scope_headroom(&scope, window, measured);
        }
        held.enter_relay(request.lease_id, &session.peer, request.request_id, now_ms)
    };
    if let Err(refusal) = entered {
        // A [`ServeAck`] and no longer a [`ServeReply`]: this is decided before
        // the body has been asked for, so the borrower still holds it and the
        // next lender may be asked. Same word on the wire, one frame earlier.
        let ack = ServeAck::Refused {
            refusal: refusal.to_wire(),
        };
        send_json(stream, session, &ack).await?;
        return Ok(());
    }
    // THE SLOT, IN A GUARD FROM HERE ON. `enter_relay` took one and every path
    // out of this function owes it back; they were three hand-placed
    // `leave_relay` calls, and the `?` on the ack write below sat between two
    // of them. A write failure there, or a poisoned ledger lock on either of
    // the later releases, stranded the lease's in-flight count for its whole
    // TTL: the borrower could not use the lease again and nothing on this Mac
    // would ever put the slot back.
    let mut slot = RelaySlot {
        ledger,
        lease_id: request.lease_id,
        returned: false,
    };

    // THE PICKER RESTRICTION, decided before anything is sent: a
    // lease scoped to a group or to named accounts may only ever be served by
    // one of them, so a scope this build cannot hold the request inside is a
    // refusal and never a full-pool serve. The slot taken by `enter_relay`
    // above is released on this path too, a refusal that leaked a slot would
    // strand the lease for its whole TTL.
    let restriction = utilization.scope_restriction(&scope);
    if restriction == ScopeRestriction::Unenforceable {
        tracing::warn!(
            peer = %session.peer.display(),
            scope = %scope,
            "peer serve: this lease's scope cannot be enforced by this build's picker, so the \
             request is refused rather than served on an account outside it"
        );
        send_json(
            stream,
            session,
            &ServeAck::Refused {
                refusal: LeaseRefusal::Unsupported,
            },
        )
        .await?;
        return Ok(());
    }

    // THE ACK: every gate this build has is past, and nothing has been sent
    // upstream. From here on a failure really is "this may have run", which is
    // exactly the fact the borrower needs and could not have before this frame
    // existed. It is written BEFORE the body is asked for, so a borrower whose
    // request was refused above still holds its body and its next lender.
    send_json(stream, session, &ServeAck::Accepted).await?;

    let body = match recv_body(stream, session).await {
        Ok(body) if body.len() == request.body_bytes => body,
        // A body that does not match the promise, or that stopped arriving.
        // The slot `enter_relay` took is released the way every other refusal
        // on this path releases it, and the borrower is told: nothing has been
        // sent upstream, so this is still "not this lender" rather than an
        // outcome it has to treat as unknown.
        outcome => {
            tracing::warn!(
                peer = %session.peer.display(),
                promised = request.body_bytes,
                sent = outcome.as_ref().map_or_default(Vec::len),
                "peer serve: the borrower's body is not the body its frame promised, so \
                 nothing was sent upstream"
            );
            send_json(
                stream,
                session,
                &ServeReply::Refused {
                    refusal: LeaseRefusal::Unsupported,
                },
            )
            .await?;
            return Ok(());
        }
    };

    // The owner's own utilization on this lease's window, before this node
    // makes the request on its own account. Read OUTSIDE the ledger lock: the
    // reader reaches the manager's accounts, and holding both locks across it
    // is how a later edit introduces a deadlock between the two.
    let before = window.map_or_default(|window| utilization.read(window));

    let started = Instant::now();
    // The request key of THIS process, which is the proxy the relay goes into:
    // in-process, so no status read decides it.
    let served = serve_on_own_account(
        upstream,
        &request,
        body,
        &restriction,
        serving.manager.request_key(),
    )
    .await;
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

    // The same windows, after. `utilization_rise` is what turns the pair into
    // one figure, and an index unmeasured on either side contributes nothing.
    // See its doc for why that is the honest direction to round in.
    let after = window.map_or_default(|window| utilization.read(window));
    let rise = utilization_rise(&before, &after);

    // The slot is released on every path out of the serve, including the error
    // one: a slot leaked by a failed relay strands the lease for its whole TTL.
    // The debit shares this one lock with it, so nothing between them can move
    // the lease this line is about to describe.
    let debited = {
        let mut held = ledger.lock().map_err(|_| anyhow!("ledger lock poisoned"))?;
        // Under the SAME lock as the debit below, which is why this one release
        // is still written by hand: nothing between them may move the lease the
        // debit is about to describe. The guard is told so it does not give the
        // slot back a second time.
        held.leave_relay(request.lease_id);
        slot.returned = true;
        // Before the debit, so this request's tokens land on the path it
        // actually arrived over: `debit` is what charges the meter, and a note
        // written after it would credit the hour's first borrow to nowhere and
        // every later one to the path of the borrow before it. The newest note
        // wins, which is what lets a borrower that moved house keep its lease
        // and change its path. Unattributable stays unattributed: `locator_from`
        // answers `None` rather than naming a locator this connection did not
        // come over, and there is no clearing call, so a borrow this build
        // could not place leaves the previous one's attribution alone.
        if let Some(path) = store
            .row(&session.peer)
            .and_then(|row| row.locator_from(from))
        {
            held.note_lease_path(request.lease_id, path);
        }
        // `is_ok` is now exactly "the upstream answered", which is what the
        // charge has to follow. It used to be narrower without saying so:
        // `serve_on_own_account` returned `Err` for a reply larger than one
        // frame, AFTER the lender's own account had paid for it, so the lease
        // was charged 0.0 and the borrower re-asked elsewhere. Every failure
        // after the answer is an answer now (see `undeliverable_answer`), so
        // the only `Err` left is one where nothing was served and nothing is
        // owed.
        if served.is_ok() {
            held.debit(request.lease_id, request.request_id, rise)
        } else {
            0.0
        }
    };

    let reply = served?;

    // THE LENDER'S LOG LINE, and it carries exactly four fields: who, which
    // window, how many bytes, how many milliseconds.
    //
    // No path, no model, no body, no header, the prompts are in this process
    // because that is what borrowing an account means, and a log file is a copy
    // of them that outlives the request. `SERVED_LINE` is the message a gate
    // greps for, named here so the gate cannot drift from it
    // (`tests/peer_boot.rs`).
    //
    // The lease ACCOUNTING (the rise this node measured and what it charged)
    // is one line below at `debug!`, not folded in here. Both are honest
    // numbers and neither is a prompt, but the four fields above are the line
    // an operator reads to answer "who used my account and for how long", and a
    // line whose field set grows is a line whose next field nobody argues
    // about. The two figures are debug because they answer a different
    // question: whether the debit is measuring anything at all.
    tracing::info!(
        peer = %session.peer.display(),
        window = ?window,
        bytes = reply.body.len(),
        ms = elapsed_ms,
        "{SERVED_LINE}"
    );
    tracing::debug!(
        peer = %session.peer.display(),
        observed_rise = rise,
        debited,
        "peer serve: what that relayed request cost this Mac's own window"
    );

    send_json(
        stream,
        session,
        &ServeReply::Served {
            status: reply.status,
            headers: reply.headers,
            body_bytes: reply.body.len(),
        },
    )
    .await?;
    send_body(stream, session, &reply.body).await?;
    Ok(())
}

/// The in-flight slot [`Ledger::enter_relay`] took, given back when it goes out
/// of scope.
///
/// Every `Ok` from `enter_relay` owes exactly one `leave_relay`, and the pairing
/// used to be three hand-placed calls on the three paths anybody had thought of.
/// The `?` on the ack write sat between two of them, so a borrower that hung up
/// while the ack was being written left the slot held, and a poisoned ledger
/// lock on either of the later releases did the same. A lease whose in-flight
/// count never comes down is refused `InFlightFull` for the rest of its TTL and
/// nothing on this Mac puts it back.
///
/// A guard cannot be forgotten by a path added later, which is the whole reason
/// it is one: the failure this replaces was not a wrong release, it was a
/// release nobody wrote.
pub(super) struct RelaySlot<'a> {
    pub(super) ledger: &'a std::sync::Mutex<Ledger>,
    pub(super) lease_id: u128,
    /// Set by the one caller that gives the slot back itself, under a lock it
    /// is already holding for the debit. Without it the guard would release a
    /// second slot that was never taken, and `leave_relay` saturates at zero,
    /// so the double release would be silent.
    pub(super) returned: bool,
}

impl Drop for RelaySlot<'_> {
    fn drop(&mut self) {
        if self.returned {
            return;
        }
        // A POISONED LOCK STILL GIVES THE SLOT BACK. `into_inner` on the
        // poisoned guard is the honest call here: the alternative is the leak
        // this type exists to stop, and a panic in a `Drop` during an unwind
        // aborts the process. The ledger's other readers keep seeing the
        // poison, so nothing is being hidden.
        let mut held = match self.ledger.lock() {
            Ok(held) => held,
            Err(poisoned) => {
                tracing::error!(
                    "peer serve: the ledger lock is poisoned, so this relay's in-flight slot is being given back through it rather than left held for the lease's whole life"
                );
                poisoned.into_inner()
            }
        };
        held.leave_relay(self.lease_id);
    }
}

/// What the lender's own proxy answered.
pub(super) struct ServedResponse {
    pub(super) status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

/// Send the relayed request through this node's OWN proxy. See
/// [`handle_serve_on`] for why that is the whole of the picker, the Bearer and
/// the bucket.
///
/// **Only [`LENDER_FORWARDED_HEADERS`] are copied onto that request.** This
/// replaced a loop that replayed every borrower-supplied header, which put a
/// borrower-chosen `cookie` (and any header a borrower invented) on the
/// lender's own outbound TLS session, `build_upstream_headers` removes
/// `authorization`, `x-api-key`, `accept-encoding` and the hop-by-hop names,
/// and `cookie` is none of those.
/// `restriction` is the scope, as the account-selection header the
/// lender's own picker reads for it.
///
/// # "Written last" was the wrong mechanism, and it was inverted
///
/// This doc used to say a borrower's own `x-tcr-group` could not widen the scope
/// because the lender writes its headers after the allowlist loop. A
/// measurement found the opposite: `RequestBuilder::header` **appends**, and
/// `src/proxy.rs` reads the scope with `headers.get(GROUP_HEADER_NAME)`, which
/// answers the FIRST value under a name. Writing last is therefore writing
/// second, and second loses.
///
/// The property is real and it rests on one thing only:
/// [`lender_forwards_header`] carries neither [`crate::proxy::GROUP_HEADER_NAME`]
/// nor [`crate::proxy::ACCOUNTS_HEADER_NAME`], so a borrower's copy is dropped
/// before the loop can append it. The loop below names the two REFUSALS
/// explicitly anyway, a second reader of the same fact, so a future widening of
/// the allowlist cannot hand a borrower the picker.
pub(super) async fn serve_on_own_account(
    upstream: &str,
    request: &ServeRequest,
    body: Vec<u8>,
    restriction: &ScopeRestriction,
    request_key: Option<&str>,
) -> Result<ServedResponse> {
    let client = crate::http_client::builder()
        .no_proxy()
        .build()
        .context("peer serve: could not build the loopback client")?;
    // THE REVIEW'S H1, STRUCTURALLY. `handle_serve_on` has already refused
    // every path `relay_path_is_routable` rejects, so this is the second half of
    // that defence and the half that holds for a caller written later: the URL
    // is built by SETTING THE PATH on a parsed base, so the host, the port and
    // the scheme come from `upstream` and nothing the borrower wrote can reach
    // them. `format!("{upstream}{path}")` could: a path that does not begin with
    // `/` turned the authority into userinfo and sent the lender's own request
    // to a host the borrower named.
    let mut url = reqwest::Url::parse(upstream)
        .context("peer serve: this Mac's own proxy base is not a URL")?;
    url.set_path(&request.path);
    // The client's own query, put back on AFTER the path gates have had the
    // path alone. Dropping it sent the lender's own account a different
    // question from the one the borrower's client asked, and answered that one
    // instead; the direct path and the carry path both keep it.
    url.set_query(request.query.as_deref());
    // And then the reading is checked against the base, but NOT because it is
    // the thing that stops a borrower-named host. This comment used to claim
    // that: "delete the `relay_path_is_routable` refusal above and feed this
    // `//169.254.169.254/latest/meta-data` and this line is what stops it".
    // Measured on this repository's pinned `url`, with `upstream =
    // "http://127.0.0.1:3456"`, the readings are asserted in
    // `set_path_keeps_the_authority_and_the_under_base_check_is_not_what_refuses`
    // (`tests/peer_lease.rs`):
    //
    //   //169.254.169.254/latest/meta-data -> .../169.254.169.254/latest/meta-data
    //   /x/../_tcr/accounts/control        -> ..../_tcr/accounts/control
    //   @169.254.169.254/latest/meta-data  -> .../@169.254.169.254/latest/…
    //
    // All three stay under the base, so this line refuses NONE of them. The
    // authority is safe because `set_path` cannot move it, and the dot-segment
    // collapse onto this Mac's own privileged `/_tcr/` route is refused by
    // `relay_path_is_routable` and by nothing else here.
    //
    // What this check does catch is an `upstream` that is not a bare origin: a
    // base carrying a path (`http://127.0.0.1:3456/api`) has that path REPLACED
    // by `set_path`, and the result is a URL outside the base this Mac was
    // configured with. That is a misconfiguration rather than an attack, and it
    // is worth a refusal rather than a silent reroute.
    let base = upstream.trim_end_matches('/');
    if !url.as_str().starts_with(base) {
        bail!(
            "peer serve: path {} builds the URL {} , which is not under this Mac's own proxy \
             at {base}; refused rather than sent",
            request.path,
            url
        );
    }
    // ONE HOP, AND THIS IS HOW THE NEXT PROXY KNOWS. The request below goes
    // into this Mac's own proxy, which is a full proxy: if this fleet is dry it
    // walks to the dry-fleet terminal and consults a fallback provider of its
    // own, so a borrowed request could be borrowed onward, to a third Mac or
    // back to the Mac that sent it. Each hop looks locally reasonable and the
    // cycle is only visible from outside. See
    // [`crate::proxy::RELAYED_HEADER_NAME`] for why presence is the whole
    // signal and why a borrower that forges it can only refuse itself.
    let mut send = client
        .post(url)
        .header(crate::proxy::RELAYED_HEADER_NAME, "1")
        .body(body);
    for (name, value) in request
        .headers
        .iter()
        .filter(|(name, _)| lender_forwards_header(name))
        // The two account-selection names, refused by name as well as by
        // absence from the allowlist. See this function's doc: `header` appends
        // and the proxy reads the first value, so a borrower's copy arriving
        // FIRST would decide the scope.
        .filter(|(name, _)| !name.eq_ignore_ascii_case(crate::proxy::GROUP_HEADER_NAME))
        .filter(|(name, _)| !name.eq_ignore_ascii_case(crate::proxy::ACCOUNTS_HEADER_NAME))
    {
        send = send.header(name, value);
    }
    match restriction {
        ScopeRestriction::Group(group) => {
            send = send.header(crate::proxy::GROUP_HEADER_NAME, group);
        }
        ScopeRestriction::Accounts(labels) => {
            // Comma-separated, and that separator is safe by construction
            // rather than by hope: `tcr_peer_wire::sanitize_label` is a
            // character whitelist (alphanumeric, `-`, `_`, `.`, space), so no
            // label a lender can hold carries a comma for this join to be
            // ambiguous about.
            send = send.header(crate::proxy::ACCOUNTS_HEADER_NAME, labels.join(","));
        }
        // `Unenforceable` never reaches here: `handle_serve_on` refuses it
        // before a byte is sent. Matched rather than `_` so a fifth arm added
        // later cannot silently fall through to an unrestricted serve.
        ScopeRestriction::Unrestricted | ScopeRestriction::Unenforceable => {}
    }
    // THIS proxy's request key, so its own relay is not counted as an unkeyed
    // caller. The only copy on the request: `LENDER_FORWARDED_HEADERS` does not
    // carry the name, so a borrower's own `x-tcr-key` never reaches the loop.
    if let Some(key) = request_key {
        send = send.header(crate::request_key::HEADER, key);
    }
    let response = send
        .send()
        .await
        .context("peer serve: this Mac's own proxy did not answer the relayed request")?;

    let status = response.status().as_u16();
    let headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_string(), value.to_string()))
        })
        .collect();
    // ===================================================================
    // EVERYTHING BELOW THIS LINE IS AFTER THE UPSTREAM ANSWERED, and that is
    // why none of it is an `Err` any more.
    //
    // An `Err` from here reached `handle_serve_on`'s `if served.is_ok()` and
    // debited 0.0: the lender's account had already paid for the request, and
    // the lease was charged nothing. Worse, the stream then died, so the
    // borrower saw no answer at all, timed out and asked another lender, which
    // ran the same request again. A reply over the frame size is not a rare
    // shape either: 65 519 bytes is any long completion.
    //
    // So both post-answer failures become an ANSWER: a small 502 the borrower
    // can read, carrying the reason. The borrower is told its request was
    // served and cannot be delivered, which is the truth, rather than being
    // left to re-ask.
    // ===================================================================
    let body = match response.bytes().await {
        Ok(bytes) => bytes.to_vec(),
        Err(err) => {
            tracing::warn!(
                error = %err,
                status,
                "peer serve: this Mac's own proxy answered and the body could not be \
                 read; the lease is charged what was measured and the borrower is told"
            );
            return Ok(undeliverable_answer(
                "the answer could not be read from this Mac's own proxy",
            ));
        }
    };
    // AN ANSWER OVER ONE FRAME IS CARRIED, NOT REFUSED. This used to return a
    // 502 for any body over `MAX_BODY_BYTES`, which is 65 519 bytes and
    // therefore any long completion: the lender's account bought the answer,
    // the lease was debited for it, and the borrower got an error with no
    // content. The reply is chunked over as many frames as it takes now
    // ([`send_body`]), and the only bound left is the one on a whole body.
    if body.len() > MAX_RELAYED_BODY_BYTES {
        tracing::warn!(
            bytes = body.len(),
            cap = MAX_RELAYED_BODY_BYTES,
            "peer serve: the answer is larger than a relayed body may be; the lease is \
             charged what was measured and the borrower is told rather than left waiting"
        );
        return Ok(undeliverable_answer(&format!(
            "the answer is {} bytes and a relayed answer carries {MAX_RELAYED_BODY_BYTES}",
            body.len()
        )));
    }
    Ok(ServedResponse {
        status,
        headers,
        body,
    })
}

/// The answer a lender sends when its own account served the request and the
/// result cannot be carried back.
///
/// **502, and never an `Err`.** The request ran, so the account paid for it and
/// the lease is charged what was measured; an `Err` here charged nothing and
/// killed the stream, which sent the borrower to the next lender with the same
/// body. A borrower that reads this knows its request was served and that this
/// answer is all it gets, which is the one thing that stops it being run twice.
///
/// # `x-should-retry: false` is the half that reaches the client
///
/// The sentence above was true of the borrower and false of the SDK sitting
/// behind it. A 502 with no header on it is retryable by default
/// (`src/proxy.rs` sets this header on every answer it means as final, and
/// `lease.rs`'s and `egress.rs`'s own "may have been billed" answers both carry
/// it), so the one request this build is certain already ran and was debited
/// was also the one the client was free to send again. The header travels the
/// same way the rest of this answer does: the borrower rebuilds the response
/// from these pairs, and this name is not hop-by-hop, so it survives.
fn undeliverable_answer(why: &str) -> ServedResponse {
    let payload = serde_json::json!({
        "type": "error",
        "error": {
            "type": "proxy_error",
            "message": format!(
                "This request was served on a peer's account and its answer could not be \
                 carried back: {why}. It was NOT served again: retrying may pay for it twice."
            ),
        },
    });
    ServedResponse {
        status: 502,
        headers: vec![
            ("content-type".to_string(), "application/json".to_string()),
            ("x-should-retry".to_string(), "false".to_string()),
        ],
        body: payload.to_string().into_bytes(),
    }
}
