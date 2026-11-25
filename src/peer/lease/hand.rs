//! `hand` mode: the owner's handed bearer, its renewal, and the borrower's spend reports.

use std::collections::HashMap;
use std::sync::Mutex;

use anyhow::{Context as _, Result};
use tcr_peer_wire::{Control, Lease, PeerId, Window};

use super::MIN_DEBIT;

/// The borrower's side of `hand` mode: the owner's short-lived
/// bearers, in memory, keyed by lease.
///
/// # Memory only, and that is the whole type
///
/// There is no `Serialize`, no `Deserialize`, no path, and nothing in `Config`
/// or in the peers file reaches it. A handed bearer is the one credential that
/// crosses a host boundary in this design, so the machine that receives it must
/// not be able to write it down: a restart loses every handed token and the
/// owner pushes fresh ones, which costs one round trip and is the correct price.
///
/// `rg -n 'HandedTokens' src/base/config.rs src/manager/mod.rs` is the containment
/// gate, and it needs a positive control beside it: an empty grep with no
/// control is a claim about the probe, not about the code.
///
/// # What the owner can and cannot take back
///
/// Revocation is "stop renewing". [`Self::forget`] drops the
/// local copy when the lease ends or the owner revokes, but a token already
/// handed over cannot be recalled: that is a property of bearer tokens, not of
/// this type, and it is why the bearer handed over is the SHORT-LIVED access
/// token and never the refresh token.
#[derive(Debug, Default)]
pub struct HandedTokens {
    by_lease: HashMap<u128, (String, i64)>,
}

impl HandedTokens {
    /// An empty store, for a test about this type and nothing else.
    ///
    /// **A store built here is not the store the request path reads, and a
    /// test that mixes the two reads it as a broken static.**
    /// [`PeerLeaseProvider::try_serve`](crate::peer::serve::fallback::FallbackProvider::try_serve)'s hand arm can only ask
    /// [`handed_tokens`], the one store this process has, and no seam hands it
    /// another. So a test that puts a bearer into its OWN `HandedTokens` and
    /// then drives the provider sees `len 1` in front of it and an empty store
    /// inside the library, microseconds apart, for the same lease id: two
    /// stores by construction, which looks exactly like one store compiled
    /// twice and is not.
    ///
    /// A test that drives the PATH therefore puts into [`handed_tokens`] and
    /// calls [`Self::forget`] on the way out, so the next test in the same
    /// binary does not inherit the bearer; `tests/peer/lending/peer_hand.rs` and
    /// `tests/peer/lending/peer_lease.rs` both do that. A test about the type itself builds
    /// one here.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the bearer the owner just pushed for `lease_id`, replacing any
    /// earlier one. A renewal is this call: the owner pushes before the old
    /// bearer expires, so the borrower never has a gap to handle.
    pub fn put(&mut self, lease_id: u128, token: String, expires_at_ms: i64) {
        self.by_lease.insert(lease_id, (token, expires_at_ms));
    }

    /// The live bearer for `lease_id`, or `None` when there is none or the one
    /// held has expired.
    ///
    /// Expiry is checked on READ rather than swept on a timer: a sweep is a
    /// second clock to get wrong, and a bearer whose deadline passed while
    /// nothing asked for it did no harm sitting in a map.
    pub fn bearer(&self, lease_id: u128, now_ms: i64) -> Option<&str> {
        self.by_lease
            .get(&lease_id)
            .filter(|(_, expires_at_ms)| *expires_at_ms > now_ms)
            .map(|(token, _)| token.as_str())
    }

    /// Drop the bearer for one lease: the lease ended, the owner revoked, or
    /// the borrower gave it back.
    pub fn forget(&mut self, lease_id: u128) -> bool {
        self.by_lease.remove(&lease_id).is_some()
    }

    /// How many leases this store holds a bearer for, expired or not. For a
    /// test and for a log line that must never print a token.
    pub fn len(&self) -> usize {
        self.by_lease.len()
    }

    /// Whether the store holds nothing at all.
    pub fn is_empty(&self) -> bool {
        self.by_lease.is_empty()
    }
}

/// The one store this process reads on the request path.
///
/// A `OnceLock` behind a `Mutex`, for the reason the fallback provider is one:
/// the alternative is threading a handle from boot through the proxy's request
/// loop, and the handle would then be a field on something `Config` can reach,
/// which is exactly the containment this type exists to keep.
pub fn handed_tokens() -> &'static Mutex<HandedTokens> {
    static HANDED: std::sync::OnceLock<Mutex<HandedTokens>> = std::sync::OnceLock::new();
    HANDED.get_or_init(|| Mutex::new(HandedTokens::new()))
}

/// The control session a hand-mode borrow can report its spend back on, per
/// lease.
///
/// # Why this exists at all
///
/// A hand-mode lease was never debited by anybody. `usage_hint` built the
/// frame and had no production caller: the owner hands a bearer over, the
/// borrower serves from its own Mac, and nothing told the owner what the lease
/// had cost, so `Ledger::debit` ran only for `serve` mode and a hand-mode
/// lease's `spent` stayed at 0.0 for its whole life. The ceiling
/// `Ledger::may_relay` enforces was therefore never reached, and the owner's
/// own `tcr peer ls` said a lease that had spent the account's window was
/// untouched.
///
/// The session that carries the hint is the one the lease was granted on: the
/// owner already keeps it open to push and renew the bearer
/// ([`read_handed_bearers`]), so the hint goes back the way the bearer came,
/// on a stream both sides have authenticated, rather than opening a second one
/// per request.
///
/// [`HandedMeter::last`] is what makes a rise measurable on this side at all.
/// The borrower never sees the owner's window except through the rate-limit
/// headers on its own answers, so the pair `usage_hint` needs is the PREVIOUS
/// answer's figure and this one's. The first answer for a lease reports
/// nothing: one reading is not a rise.
#[derive(Debug)]
struct HandedMeter {
    /// Where a hint frame is queued for the reader task that owns the stream.
    sink: tokio::sync::mpsc::UnboundedSender<Control>,
    /// The window utilization the previous answer on this lease reported.
    last: Option<f64>,
}

/// Every live hand-mode lease's meter, keyed by lease id.
///
/// Process-local and unexported, for [`handed_tokens`]' reason: a hand-mode
/// lease's session is a live socket, not configuration, and a handle threaded
/// from boot through the request loop would put it on something `Config` can
/// reach.
fn handed_meters() -> &'static Mutex<HashMap<u128, HandedMeter>> {
    static METERS: std::sync::OnceLock<Mutex<HashMap<u128, HandedMeter>>> =
        std::sync::OnceLock::new();
    METERS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register the channel a lease's hints go out on. Replaces any earlier one:
/// a lease is granted on one session, and a second grant of the same id would
/// be a lender contradicting itself.
pub(super) fn register_handed_meter(
    lease_id: u128,
    sink: tokio::sync::mpsc::UnboundedSender<Control>,
) {
    if let Ok(mut meters) = handed_meters().lock() {
        // THE BASELINE SURVIVES the channel being replaced. A lease whose
        // session is re-established, or which is re-asked for inside its own
        // life, is the same lease against the same window, and a meter reset to
        // `None` here makes the next answer free: the first reading is not a
        // rise, so it charges nothing at all. Only the sink is new.
        let last = meters.get(&lease_id).and_then(|meter| meter.last);
        meters.insert(lease_id, HandedMeter { sink, last });
    }
}

/// Take the owner's own utilization, off a [`Control::Handoff`], as the figure
/// this lease's next answer is measured against.
///
/// **Monotone: `max`, never a replacement.** With `max_inflight` above one, two
/// answers can land out of order, and a baseline moved BACKWARDS by the older
/// of them turns the next answer's rise into a second charge for quota already
/// billed. A window that genuinely reset reads as a fall, which is the owner's
/// windfall exactly as [`Ledger::debit`](crate::peer::lease::Ledger::debit) says, and never a credit here.
pub(super) fn note_handed_baseline(lease_id: u128, utilization: f64) {
    if !utilization.is_finite() {
        return;
    }
    if let Ok(mut meters) = handed_meters().lock()
        && let Some(meter) = meters.get_mut(&lease_id)
    {
        meter.last = Some(meter.last.map_or(utilization, |last| last.max(utilization)));
    }
}

/// Drop a lease's meter when its session ends, so this map holds only live
/// leases. Same rule as [`HandedTokens::forget`] and for the same reason.
pub(super) fn forget_handed_meter(lease_id: u128) {
    if let Ok(mut meters) = handed_meters().lock() {
        meters.remove(&lease_id);
    }
}

/// Report what one hand-mode answer cost, on the lease's own window.
///
/// `utilization` is what the answer's own rate-limit headers said; `None` (an
/// answer that reported nothing about the window) leaves the meter untouched,
/// because "the window did not move" and "this answer said nothing about it"
/// are different facts and charging the second as the first is how a lease
/// stops tracking anything.
///
/// Never blocks and never fails the request: a hint that cannot be sent is a
/// debit the owner does not learn about, which is worth a line and is not worth
/// failing a served request over.
pub(crate) fn report_handed_spend(lease_id: u128, utilization: Option<f64>) {
    let Some(utilization) = utilization else {
        return;
    };
    let Ok(mut meters) = handed_meters().lock() else {
        return;
    };
    let Some(meter) = meters.get_mut(&lease_id) else {
        return;
    };
    let before = meter.last;
    // `max`, not `replace`, for [`note_handed_baseline`]'s reason: with
    // `max_inflight` above one the answers arrive in whatever order the network
    // hands them over, and a baseline pushed backwards by the older one bills
    // the same quota twice on the next answer.
    meter.last = Some(before.map_or(utilization, |last| last.max(utilization)));
    let Some(hint) = usage_hint(lease_id, before, Some(utilization)) else {
        return;
    };
    if meter.sink.send(hint).is_err() {
        tracing::debug!(
            "peer hand: the lease's control session has ended, so this spend is not \
             reported to its owner"
        );
    }
}

/// The utilization one window's rate-limit header reported on a response, or
/// `None` when this answer said nothing about that window.
///
/// The header names are `src/records/quota/mod.rs`'s own; read here rather than through
/// `Quota::update_from_headers` because that merges into an account's quota,
/// and a borrower has no account of the owner's to merge into. It is one
/// figure, for one window, off one answer.
pub(crate) fn reported_utilization(head: &[(String, String)], window: Window) -> Option<f64> {
    let name = match window {
        Window::FiveHour => "anthropic-ratelimit-unified-5h-utilization",
        Window::SevenDay => "anthropic-ratelimit-unified-7d-utilization",
        Window::SevenDayOi => "anthropic-ratelimit-unified-7d_oi-utilization",
        Window::Unknown => return None,
    };
    head.iter()
        .find(|(header, _)| header.eq_ignore_ascii_case(name))
        .and_then(|(_, value)| value.parse::<f64>().ok())
        .filter(|utilization| utilization.is_finite())
}

/// Whether the owner should push a [`Control::Handoff`] for this lease right
/// now, and what it carries.
///
/// The three refusals are the whole of "only, and stops at":
///
/// - a `Serve` grant is never handed anything, whatever else is true;
/// - a lease past its `until` is over, so the owner stops renewing (that IS the
///   revoke, and there is no recall frame);
/// - a bearer the owner cannot produce for the grant's scope is not a reason to
///   send an empty frame.
///
/// The caller is responsible for the fourth condition, which this function
/// cannot see: the frame goes out only on an `IK`-authenticated CONTROL session
/// to the grantee of `lease`. That is a property of the socket, not of the
/// arguments, and `Ledger::grantee_of` is what the sender checks it against.
pub fn handoff_for(
    mode: crate::peer::config::LendMode,
    lease: &Lease,
    bearer: Option<crate::manager::HandedCredential>,
    now_ms: i64,
) -> Option<Control> {
    use crate::peer::config::LendMode;
    if mode != LendMode::Hand {
        return None;
    }
    if lease_has_ended(lease, now_ms) {
        return None;
    }
    let crate::manager::HandedCredential {
        access_token,
        expires_at_ms,
        utilization,
    } = bearer?;
    // A bearer that has already expired is not a handoff, it is a frame that
    // costs the borrower a request to discover is useless. The lease's own
    // expiry is checked above and they are different clocks: a live lease can
    // sit beside a credential whose refresh has not happened yet, and this
    // function used to hand that one over.
    if expires_at_ms <= now_ms {
        return None;
    }
    Some(Control::Handoff {
        lease_id: lease.lease_id,
        access_token: tcr_peer_wire::HandoffToken::new(access_token),
        expires_at_ms,
        // THE BASELINE, and the reason this frame carries a figure at all. The
        // borrower can only see the owner's window through the rate-limit
        // headers on its own answers, so a rise needs a reading from BEFORE the
        // first of them; without one the first answer on every lease was free,
        // and a borrower that re-asked per request was never charged for
        // anything. See [`HandedMeter`] and [`usage_hint`].
        utilization: Some(utilization),
    })
}

/// Has this lease stopped, by its own expiry or by the lending's `until`?
///
/// `until` is in unix SECONDS (the peers file and the wire both say so) and
/// everything else here is milliseconds, so the comparison converts once, in
/// one place, rather than at each of the callers that would otherwise each pick
/// a unit.
pub fn lease_has_ended(lease: &Lease, now_ms: i64) -> bool {
    if lease.expires_at_ms <= now_ms {
        return true;
    }
    lease.until.is_some_and(|until_s| {
        // `until` is unix SECONDS and unsigned; a value too large for an
        // `i64` of milliseconds is a date no clock will reach, which is
        // "not ended" and never a panic.
        i64::try_from(until_s.saturating_mul(1_000)).is_ok_and(|end_ms| end_ms <= now_ms)
    })
}

/// Should the owner push a FRESH bearer before the one it last handed over
/// stops working?
///
/// `handed_expires_at_ms` is what the last [`Control::Handoff`] carried, and
/// `None` means none was ever sent. The lead time is [`HANDOFF_RENEW_LEAD_MS`].
///
/// Read rather than scheduled, for the reason the expiry check in
/// [`HandedTokens::bearer`] is read rather than swept: a laptop sleeps, and a
/// timer that fired while the lid was shut is a renewal nobody received.
pub const fn handoff_renewal_due(handed_expires_at_ms: Option<i64>, now_ms: i64) -> bool {
    match handed_expires_at_ms {
        None => true,
        Some(expires_at_ms) => expires_at_ms - now_ms <= HANDOFF_RENEW_LEAD_MS,
    }
}

/// What one session last handed over for one lease.
///
/// # Why the token's fingerprint is here and not just its expiry
///
/// [`handoff_renewal_due`] is true for the whole lead time, and the control
/// loop asks it once a second. With only the expiry recorded, an owner whose
/// credential had not been refreshed yet re-sent the SAME bearer on every poll
/// for the last five minutes of its life: about three hundred identical frames
/// per lease, each one a Noise message the borrower decodes and stores over an
/// identical copy. The renewal is due for as long as the token has not
/// changed, so "due" cannot be the whole test; what makes a handoff worth
/// sending is that the bearer is DIFFERENT from the one the borrower holds,
/// and that is what the fingerprint answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HandedBearer {
    /// When the bearer that went out stops working, off the `Handoff` frame.
    pub expires_at_ms: i64,
    /// [`bearer_fingerprint`] of the token that went out.
    pub fingerprint: u64,
}

/// A fingerprint of a bearer token, for telling one from another without
/// keeping a second copy of the credential.
///
/// In-memory only, for the life of one session, which is why the standard
/// library's hasher is enough: nothing compares a fingerprint across processes
/// or writes one anywhere, and the only question ever asked of it is "is this
/// the same token I already sent on this session". A credential never reaches
/// a log, a file or the wire through this: a `u64` is not the token, and it is
/// not treated as a secret either, because it is never sent.
/// Should the owner send `next` on a session that last sent `last`?
///
/// Two questions, and both have to be yes: the renewal is DUE
/// ([`handoff_renewal_due`]), and the bearer is actually different from the one
/// that went out. The second is what keeps one push per token instead of one
/// per poll: the lead time is five minutes and the control loop polls every
/// second, so "due" alone re-sends an unchanged token about three hundred
/// times.
///
/// A pure function rather than a condition inline in the loop, because the
/// claim worth measuring is about a SEQUENCE of polls, and a test can run five
/// simulated minutes of them through this in a millisecond.
pub fn handoff_push_is_due(last: Option<HandedBearer>, next: HandedBearer, now_ms: i64) -> bool {
    if !handoff_renewal_due(last.map(|sent| sent.expires_at_ms), now_ms) {
        return false;
    }
    last.is_none_or(|sent| sent.fingerprint != next.fingerprint)
}

pub fn bearer_fingerprint(access_token: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    access_token.hash(&mut hasher);
    hasher.finish()
}

/// The borrower's quota hint for one hand-mode response, or `None` when there
/// is nothing honest to report.
///
/// # Read from the response, not from a counter
///
/// `before` and `after` are the window's utilization as the rate-limit headers
/// reported it either side of one request, which is the same evidence the owner
/// debits from in `serve` mode ([`crate::peer::serve::utilization_rise`]) read
/// on the machine that actually got the response. A response with no rate-limit
/// header reports `None` rather than zero: "the window did not move" and "this
/// response said nothing about the window" are different facts, and charging
/// the second as the first is how a lease stops tracking anything.
///
/// A window that RESET under the request reads as a fall. That is `None` too:
/// a reset is the owner's windfall, exactly as [`Ledger::debit`](super::Ledger::debit) says, and a
/// borrower has no business reporting a negative.
pub fn usage_hint(lease_id: u128, before: Option<f64>, after: Option<f64>) -> Option<Control> {
    let (before, after) = (before?, after?);
    if !before.is_finite() || !after.is_finite() {
        return None;
    }
    let rise = after - before;
    if rise <= 0.0 {
        return None;
    }
    Some(Control::UsageHint {
        lease_id,
        // THE FLOOR. The utilization header moves in steps, so a request whose
        // cost is smaller than one step reports a rise of zero on the answer
        // that paid for it and the whole of it later, on some other lease's
        // answer or on none at all. The serve path charges [`MIN_DEBIT`] for
        // exactly this case (`Ledger::debit`), and a hand lease that did not
        // was a lease a small-request borrower could hold open indefinitely.
        //
        // It floors a rise that is already POSITIVE and never invents one: an
        // answer that moved nothing is still free, which is what keeps "it can
        // only raise `spent`" honest rather than turning every request into a
        // charge.
        spent: rise.max(MIN_DEBIT),
    })
}

/// The baseline a [`Control::Handoff`] brings, or the line an operator has to
/// read instead.
///
/// # A handoff with no baseline is refused, and the lender is NAMED
///
/// The figure is what a borrowed answer's spend is a rise above
/// ([`usage_hint`]), so a bearer that arrives without one is a bearer whose
/// every answer this Mac would take for free: the owner's ledger would show a
/// lease that was never spent and its ceiling would never bind. Only a build
/// OLDER than this one leaves it out, which is why the refusal names the Mac to
/// update rather than reporting a protocol error nobody can act on. The lease
/// itself stands and the borrow falls back to the lender's own serve path.
///
/// A function returning the line, rather than a `warn!` inline, so a test can
/// assert that the peer is named: "refused" and "refused in a way the operator
/// can act on" are different claims and the second one needs a value.
pub fn handed_baseline(lender: &PeerId, utilization: Option<f64>) -> Result<f64, String> {
    match utilization {
        Some(utilization) if utilization.is_finite() => Ok(utilization),
        _ => Err(format!(
            "peer lease: {} handed this Mac a bearer without the utilization its spend would \
             be measured against, which is what a build older than this one sends, so the \
             bearer is refused and this borrow stays on that Mac's own path until it is \
             updated",
            lender.display()
        )),
    }
}

/// How long before a handed bearer expires the owner pushes the next one.
///
/// Five minutes, which is the same order as the lease ttl the defaults use
/// (300 s) and comfortably longer than a LAN round trip plus a token refresh.
/// It is a choice, not a measurement: the figure that would settle it is the
/// observed spread of `expires_at` on a refreshed credential, which nobody
/// has measured.
pub const HANDOFF_RENEW_LEAD_MS: i64 = 5 * 60 * 1_000;

/// When this lease stops being a lease, on the borrower's own clock.
///
/// The earlier of its renewal deadline and the lending's `until`. Both are
/// already the pair [`lease_has_ended`] compares; this is the same question
/// asked as an INSTANT, because a reader that has to stop at the end needs to
/// know when, not only whether.
fn lease_ends_at_ms(lease: &Lease) -> i64 {
    let until_ms = lease
        .until
        .and_then(|until_s| i64::try_from(until_s.saturating_mul(1_000)).ok());
    match until_ms {
        Some(until_ms) => lease.expires_at_ms.min(until_ms),
        None => lease.expires_at_ms,
    }
}

/// Hold the CONTROL session open and take the owner's bearer, and every
/// renewal after it, into this process's [`handed_tokens`] store.
///
/// # This is the whole of the borrower's side of `hand` mode
///
/// The owner sends [`Control::Handoff`] on the session that granted the lease
/// and pushes a fresh one before the old expires. Both halves existed and
/// nothing connected them: `request_lease` read one frame and returned, so the
/// session was dropped with the bearer still in flight and
/// [`HandedTokens::put`] had no production caller at all. From the outside that
/// is indistinguishable from a lender that never sent one.
///
/// # Stopping
///
/// Three ways, and these are all three. The stream ends, which
/// is the owner closing or going away. The lease reaches its end, renewal
/// deadline or `until`, whichever is sooner, which is the one timeout here and
/// is why it is measured once against [`lease_ends_at_ms`] rather than sliced:
/// a read cancelled part way through a frame would leave the session out of
/// step, and after this one fires nothing reads the stream again. And the owner
/// simply stops renewing, which IS the revoke: the bearer held expires on its
/// own deadline, [`HandedTokens::bearer`] stops answering it, and no recall
/// frame exists because a token already on another machine cannot be taken
/// back.
///
/// Whichever way it ends, the copy goes: [`HandedTokens::forget`] runs on the
/// way out rather than waiting for a read that may never come.
pub(super) async fn read_handed_bearers(
    mut stream: crate::peer::noise::PeerStream,
    mut session: crate::peer::noise::PeerSession,
    lease: Lease,
    lender: PeerId,
) {
    // THE HINT CHANNEL, registered before the first read: this session is the
    // one a hand-mode borrow reports its spend on, and a borrow served between
    // the bearer arriving and this task being ready would otherwise find no
    // meter and report nothing. See [`HandedMeter`].
    let (hints, mut hints_rx) = tokio::sync::mpsc::unbounded_channel();
    register_handed_meter(lease.lease_id, hints);

    // THE FRAME READER, and it is what makes the `select!` below safe at all.
    // `noise::recv_control` is not cancel safe: a hint arriving while a Handoff
    // is half read dropped that read with bytes already taken off the socket,
    // and the next read then met the tail of one frame as a length prefix, so
    // the Noise stream desynced and this borrower forgot its bearer for the
    // rest of the lease. `noise::FrameReader` holds the part-read bytes in the
    // loop's own state instead of in the cancelled future, which is the failure
    // its own doc describes.
    let mut frames = crate::peer::noise::FrameReader::new();

    let remaining = lease_ends_at_ms(&lease).saturating_sub(crate::now_ms());
    if remaining > 0 {
        let until_the_lease_ends =
            std::time::Duration::from_millis(u64::try_from(remaining).unwrap_or(0));
        let reader = async {
            loop {
                let frame: Result<Control> = tokio::select! {
                    // BIASED, so a queued hint is never starved by a lender
                    // that talks constantly: the spend is what keeps the
                    // owner's ceiling binding, and a hint that waits for a
                    // quiet session is a debit that arrives after the lease is
                    // over.
                    biased;
                    Some(hint) = hints_rx.recv() => {
                        if let Err(err) =
                            crate::peer::noise::send_control(&mut stream, &mut session, &hint).await
                        {
                            tracing::debug!(
                                peer = %lender.display(),
                                error = %err,
                                "peer hand: this lease's spend could not be reported to its \
                                 owner, so the owner's own ledger is behind"
                            );
                            return;
                        }
                        continue;
                    }
                    bytes = frames.recv_encrypted(&mut stream, &mut session.transport) => {
                        bytes.and_then(|bytes| {
                            serde_json::from_slice::<Control>(&bytes)
                                .context("peer lease: a frame on this lease's session did not parse")
                        })
                    }
                };
                let frame: Control = match frame {
                    Ok(frame) => frame,
                    Err(err) => {
                        tracing::debug!(
                            peer = %lender.display(),
                            error = %err,
                            "peer lease: the lender's control session ended, so no further \
                             bearer arrives on it"
                        );
                        return;
                    }
                };
                match frame {
                    Control::Handoff {
                        lease_id,
                        access_token,
                        expires_at_ms,
                        utilization,
                    } => {
                        if lease_id != lease.lease_id {
                            // A bearer for a lease this session did not mint is
                            // refused rather than stored: the store is keyed by
                            // lease id and taking this one would let a lender
                            // overwrite the bearer of a lease it does not hold.
                            tracing::warn!(
                                peer = %lender.display(),
                                "peer lease: a bearer arrived for a lease this session did \
                                 not mint, so it is dropped"
                            );
                            continue;
                        }
                        // A handoff with no baseline is refused and the lender
                        // is named: see [`handed_baseline`], which owns the
                        // whole of that rule and the line.
                        let baseline = match handed_baseline(&lender, utilization) {
                            Ok(baseline) => baseline,
                            Err(line) => {
                                tracing::warn!("{line}");
                                continue;
                            }
                        };
                        if let Ok(mut held) = handed_tokens().lock() {
                            held.put(lease_id, access_token.reveal().to_string(), expires_at_ms);
                            // The baseline the next answer is measured
                            // against, taken monotonically: a renewal
                            // carries the owner's window as it stands now,
                            // and a later frame can only move it forward.
                            note_handed_baseline(lease_id, baseline);
                            // The deadline and the lease, never the token:
                            // this line is the one an operator reads while
                            // a hand-mode borrow is running.
                            tracing::info!(
                                peer = %lender.display(),
                                expires_at_ms,
                                "peer lease: the lender handed this Mac a bearer for the \
                                 lease it just granted"
                            );
                        } else {
                            tracing::error!(
                                "peer lease: the handed-token store's lock is poisoned, so \
                                 this borrow falls back to the lender's own path"
                            );
                            return;
                        }
                    }
                    other => {
                        // Not fatal: the session is the lender's to speak on and
                        // a frame this build does not expect here costs nothing
                        // to skip. `Handoff`'s own `Debug` redacts the bearer,
                        // so this line is safe whatever arrived.
                        tracing::warn!(
                            peer = %lender.display(),
                            frame = ?other,
                            "peer lease: an unexpected frame arrived on a lease's control \
                             session; ignoring it"
                        );
                    }
                }
            }
        };
        if tokio::time::timeout(until_the_lease_ends, reader)
            .await
            .is_err()
        {
            tracing::debug!(
                peer = %lender.display(),
                "peer lease: this lease reached its end, so the owner's renewals stop being \
                 read and the bearer held for it is dropped"
            );
        }
    }
    if let Ok(mut held) = handed_tokens().lock() {
        held.forget(lease.lease_id);
    } else {
        tracing::error!(
            "peer lease: the handed-token store's lock is poisoned, so an ended lease's bearer \
         is left in memory until restart"
        );
    }
    // The meter goes with the bearer: nothing can be reported on a session
    // that has ended, and a meter left behind would hold a sender nobody reads
    // for the life of the process.
    forget_handed_meter(lease.lease_id);
}
