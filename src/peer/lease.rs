//! The lease: a budget with a deadline, and the guard that outranks it.
//!
//! A lease is `{window, unit, fraction, ttl, max_inflight}`, which window, how
//! much of it, for how long, how many requests at once. The **lender's** ledger
//! is authoritative and persisted; the borrower's copy is a cached hint and
//! every surface that renders it says so.
//!
//! # Who debits
//!
//! The lender, exactly once, on observing the response, from the utilization
//! rise it measures on its own account. The borrower never debits. A retried or
//! diamond-delivered relay debits once, keyed on the request id.
//!
//! [`MIN_DEBIT`] is charged when the observed rise is zero, because the
//! granularity of the upstream utilization header is undocumented: "no rise"
//! and "a rise below the reporting step" are the same bytes, and a lease that
//! charges nothing for them never ends. **This is the one constant here that is
//! a guess rather than a measurement**, and the check that settles it is
//! reading consecutive header values off live responses and looking at the step
//! size. Do that before trusting the number.
//!
//! # A lease can never spend the owner's guard
//!
//! [`may_relay`] answers in a fixed order, expired, spent, then owner-guard,
//! and **the third fires on its own even with budget left on the lease**. It
//! inherits the failure mode the main config already records for the same
//! headers: they lag, upstream is the oracle, and it answers 200s for accounts
//! it is about to bench. That is exactly why a guard band exists rather than
//! the raw threshold.
//!
//! # `max_inflight` is not a nicety
//!
//! The debit reads lagging headers, so a lease granted at a small fraction with
//! unbounded concurrency can be overdrawn inside the lag window before the
//! first debit lands. The cap bounds the overdraft to a known number of
//! requests rather than to however many the borrower can open.

use tcr_peer_wire::{Lease, LeaseGrant, LeaseRefusal, LeaseRequest, LeaseUnit, LendScope, Window};

use crate::peer::config::{LendGrant, PeerRow, PeerStore};

mod hand;
mod ledger;
mod lend_end;
mod provider;

#[cfg(test)]
mod parse_lend_end_tests;
#[cfg(test)]
mod tests;

pub use hand::{
    HANDOFF_RENEW_LEAD_MS, HandedBearer, HandedTokens, bearer_fingerprint, handed_baseline,
    handed_tokens, handoff_for, handoff_push_is_due, handoff_renewal_due, lease_has_ended,
    serve_on_handed_bearer, usage_hint,
};
pub use ledger::{
    Ledger, MAX_LEASES_PER_PEER, REQUEST_DEDUP_CAPACITY, REQUEST_DEDUP_TTL_MS, SERVED_CAPACITY,
    SERVED_PER_LEASE_CAPACITY, SERVED_TTL_MS, ScopeRestriction, WindowUtilization,
};
pub use lend_end::parse_lend_end;
pub use provider::{PeerLeaseProvider, lenders_in_ask_order, nothing_left_this_mac, request_lease};

/// Why one RELAYED REQUEST was refused, as opposed to why a LEASE was.
///
/// [`LeaseRefusal`] is the wire vocabulary and every variant of it describes the
/// lease. `max_inflight` is not about the lease, the lease is live, funded and
/// inside the owner's guard, it is about this request arriving while n others
/// are still running, and the honest answer names the cap rather than claiming
/// the lease is spent.
///
/// Typed here rather than as a sixth [`LeaseRefusal`] variant because that enum
/// is in `tcr_peer_wire` and has no such variant. **The wire needs the
/// variant**: until it has one, a lender can enforce the cap locally but cannot
/// tell the borrower which of the two happened, and a borrower that reads
/// `lease-spent` for a full pipe will stop asking when it should retry in a
/// moment. Reported, not worked around.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayRefusal {
    /// The lease itself said no. Pass straight to the borrower.
    Lease(LeaseRefusal),
    /// The lease is fine; this request is the (n+1)th at once.
    TooManyInflight {
        /// [`Lease::max_inflight`], so the refusal says what the cap is.
        max: u8,
    },
    /// **This `(lease_id, request_id)` pair has already been relayed here.**
    ///
    /// The review's H2: [`Ledger::debit`] was idempotent per pair and the SERVE
    /// was not, so one replayed `request_id` was served on the lender's account
    /// as many times as the borrower liked while [`Lease::spent`] moved exactly
    /// once, and `lease.spent >= budget` could then never become true. The
    /// lease was unbounded in the one dimension it exists to bound.
    ///
    /// Refused rather than served-and-charged-again: the pair is the accounting
    /// key, so a second serve under one key has no honest charge, and a borrower
    /// that really wants the request served again has a fresh id to ask with.
    ///
    /// **The wire word is [`LeaseRefusal::Unsupported`] and it is the wrong
    /// word**, reported rather than worked around: `LeaseRefusal` lives in
    /// `tcr_peer_wire`, and nothing here adds a variant to it. The variant
    /// that belongs there is
    /// `LeaseRefusal::RequestReplayed`, terminal like `Unsupported` (do not
    /// retry THIS id) rather than "retry in a moment" like
    /// [`LeaseRefusal::InFlightFull`].
    Replayed,
    /// The lease is live and funded, and it was granted to a DIFFERENT peer.
    ///
    /// The review's M1: every ledger entry point keyed on the lease id alone,
    /// so a lease was a bearer token, any pinned peer holding `inspect` could
    /// spend any lease id it learned, on the amount another peer negotiated.
    /// The authorization was the secrecy of a random number, and
    /// [`Ledger::grant`] already holds the authenticated peer one line before
    /// it mints the id.
    ///
    /// [`LeaseRefusal::LeaseExpired`] on the wire, deliberately: it is the
    /// answer this ledger already gives for a lease id it does not hold (see
    /// [`Ledger::may_relay`]), and a peer spending somebody else's id must not
    /// be told the difference between "that is not yours" and "there is no such
    /// lease", the first sentence is an oracle for lease ids.
    NotTheGrantee,
}

impl RelayRefusal {
    /// The wire word for this refusal.
    ///
    /// [`LeaseRefusal::InFlightFull`] exists for the second arm, added to
    /// `crates/tcr-peer-wire` for exactly this. Before it, a lender could
    /// enforce the cap and had no way to
    /// say so, and a borrower reading `lease-spent` for a full pipe stops asking
    /// when it should retry in a moment.
    pub const fn to_wire(self) -> LeaseRefusal {
        match self {
            Self::Lease(refusal) => refusal,
            Self::TooManyInflight { .. } => LeaseRefusal::InFlightFull,
            // See [`Self::Replayed`]: the honest variant does not exist on the
            // wire yet and `Unsupported` is the closest terminal answer.
            Self::Replayed => LeaseRefusal::Unsupported,
            // See [`Self::NotTheGrantee`]: the SAME answer as a lease id this
            // ledger never minted, on purpose.
            Self::NotTheGrantee => LeaseRefusal::LeaseExpired,
        }
    }
}

/// The spendable budget of a lease, as a fraction, or `None` for a unit this
/// build refuses.
///
/// [`LeaseUnit::Tokens`] is defined on the wire and refused here, deliberately:
/// it is real for an api-key account and no account in this fleet lends one yet,
/// so the refusal is typed instead of arriving as a surprise comparison between
/// a token count and a utilization fraction.
const fn fraction_budget(unit: LeaseUnit) -> Option<f64> {
    match unit {
        LeaseUnit::Fraction(fraction) => Some(fraction),
        LeaseUnit::Tokens(_) | LeaseUnit::Unknown => None,
    }
}

/// Is `now` inside `schedule`, and if not, the refusal to
/// answer with.
///
/// `None` on `schedule`, no schedule at all, what every grant written before
/// schedules existed means, is always permitted, same as `Schedule::always()` would
/// be. A parameter rather than a read off [`LendGrant`], mirroring
/// [`clamp_to_grant`]'s own `end: Option<u64>`: this stays pure and testable
/// at any instant without a peers file, and the one caller
/// ([`Ledger::grant`]) is the one that reads the grant's own field.
///
/// Called from [`Ledger::grant`], which reads
/// [`crate::peer::config::LendGrant::schedule`] off the grant it picked and
/// asks this before it measures the fleet: a Mac outside its lending hours
/// has nothing to measure for.
pub fn schedule_refusal(
    schedule: Option<&crate::peer::schedule::Schedule>,
    now: time::OffsetDateTime,
) -> Option<LeaseRefusal> {
    match schedule {
        Some(schedule) if !schedule.contains(now) => Some(LeaseRefusal::OutsideSchedule),
        _ => None,
    }
}

/// When the window `schedule` is open inside, at `now`, stops being open,
/// as unix milliseconds; `None` for a schedule with no intraday edge at all
/// ([`crate::peer::schedule::Schedule::always`]).
///
/// **Assumes `now` is already inside `schedule`**
/// ([`schedule_refusal`] answered `None` for it): the one caller
/// ([`Ledger::grant`]) asks this right after that check, on the same
/// reading. A `between` with no midnight crossing closes today at `end`; a
/// crossing window closes today at `end` for the tail half (past midnight,
/// before `end`) and tomorrow at `end` for the head half (past `start`,
/// before midnight). `between: None` with a `days` filter closes at the next
/// local midnight, the one intraday edge a day-only schedule has.
///
/// Built here rather than as a method on [`crate::peer::schedule::Schedule`]:
/// `Hhmm`'s and `Schedule`'s own fields are public, this is the only caller,
/// and the one thing this fix touches outside `lease.rs`'s own module is a
/// wire refusal that already existed, so the close-time arithmetic stays
/// beside the clamp ([`clamp_to_grant`]'s `schedule_close_ms`) it feeds
/// rather than widening `schedule.rs`, which this unit does not own.
fn schedule_closes_at_ms(
    schedule: &crate::peer::schedule::Schedule,
    now: time::OffsetDateTime,
) -> Option<i64> {
    let offset = time::UtcOffset::local_offset_at(now).unwrap_or(time::UtcOffset::UTC);
    let local = now.to_offset(offset);
    let minute = u16::from(local.hour()) * 60 + u16::from(local.minute());
    let (close_time, next_day) = match schedule.between {
        None => {
            // No time-of-day restriction, only a `days` filter: the one edge
            // that filter has is the day changing at local midnight.
            schedule.days.as_ref()?;
            (time::Time::MIDNIGHT, true)
        }
        Some((start, end)) => {
            let (start_m, end_m) = (start.minute_of_day(), end.minute_of_day());
            let close_time = time::Time::from_hms(end.hour(), end.minute(), 0).ok()?;
            match start_m.cmp(&end_m) {
                std::cmp::Ordering::Less => (close_time, false),
                std::cmp::Ordering::Greater => {
                    if minute < end_m {
                        (close_time, false)
                    } else {
                        (close_time, true)
                    }
                }
                // Never open (see `Schedule::contains_at_local`), so `now`
                // cannot honestly be inside it under this function's own
                // precondition; answered rather than reached.
                std::cmp::Ordering::Equal => return None,
            }
        }
    };
    let date = if next_day {
        local.date().next_day()?
    } else {
        local.date()
    };
    let close = date.with_time(close_time).assume_offset(local.offset());
    i64::try_from(close.unix_timestamp_nanos() / 1_000_000).ok()
}

/// What [`Ledger::grant`] answers: the frame the borrower gets, and the
/// operator's own grant that funded it.
///
/// # Why the funding grant leaves the function at all
///
/// A review found the caller re-deriving it. `listener.rs` read the
/// lease's MODE back out of the peers file with
/// `row.lend.iter().find(|g| g.window == minted.window)`, a window-only match,
/// while the lease had been funded by
/// [`crate::peer::config::PeerRow::grant_for`], which also skips a grant that
/// has ENDED and one whose scope this Mac's picker cannot hold. A row carrying
/// an ended `hand` grant and a live `serve` grant on the same window therefore
/// minted from the serve grant and handed the owner's bearer over under the
/// ended hand one.
///
/// So the funding grant is returned rather than looked up twice: one value,
/// read where it was decided, and no second filter that can disagree with the
/// first.
///
/// [`Self::funding`] is the grant this answer was CUT FROM, whether or not a
/// lease came out of it. A refusal reached before a grant was even picked
/// carries `None`.
#[derive(Debug, Clone)]
pub struct Granted {
    /// The answer that goes on the wire, unchanged: this type adds nothing to
    /// the frame and the frame carries no policy of the lender's.
    pub answer: LeaseGrant,
    /// The `LendGrant` the lease was funded by, as
    /// [`crate::peer::config::PeerRow::grant_for`] picked it. Never sent
    /// anywhere: it names the lender's own scope.
    pub funding: Option<LendGrant>,
    /// What this lease draws from, the same value [`Ledger::grant`] recorded
    /// beside the lease, and the argument
    /// [`crate::manager::Manager::handoff_bearer`] resolves an account out of.
    ///
    /// # Why it is carried out rather than read back
    ///
    /// The handoff arm used to re-read it with
    /// `serving.ledger.lock().map(|held| held.scope_of(id)).unwrap_or_default()`,
    /// so a POISONED ledger lock quietly became [`LendScope::All`] and the
    /// owner's bearer was then resolved against every account on the Mac
    /// instead of the ones the operator lent. A second lock is what made the
    /// silent fallback expressible at all, so it is gone: the scope leaves the
    /// function that decided it, in the same value as the answer and the
    /// funding grant.
    pub scope: LendScope,
}

impl Granted {
    /// A refusal decided before any grant was picked, so there is nothing that
    /// funded it.
    const fn unfunded(refusal: LeaseRefusal) -> Self {
        Self {
            answer: LeaseGrant {
                lease: None,
                refusal: Some(refusal),
            },
            funding: None,
            scope: LendScope::All,
        }
    }

    /// The mode the lease was funded under, which is the one question the
    /// handoff arm asks. [`crate::peer::config::LendMode::Serve`] when no grant
    /// funded it, because the refusal path hands nothing over either way.
    pub fn mode(&self) -> crate::peer::config::LendMode {
        self.funding
            .as_ref()
            .map_or_else(Default::default, |grant| grant.mode)
    }
}

/// The whole of the lender's grant arithmetic, as a pure function: what the
/// borrower asked for, clamped by what this peer was granted, clamped again by
/// what the owner's guard band actually leaves.
///
/// Pure and separate from [`Ledger::grant`] so it can be tested without a peers
/// file and without a random source, and so a reader can see that the clamps are
/// the only decision being made. The order is the point, the narrowest of the
/// three always wins, and `lendable` is last because it is the only one measured
/// rather than configured.
///
/// `inspect` is the LENDER's side of the two opt-ins. The borrower's
/// `allow.disclose` is checked on the borrower, before it ever asks, which is
/// why it is not a parameter here: a lender that enforced both would be
/// pretending it can see the borrower's config.
///
/// `max_inflight` is passed through as the operator configured it, `0` included:
/// a `0` grant mints a lease that [`Ledger::enter_relay`] refuses every time
/// with the cap named, which is what an operator who wrote `0` asked for. It is
/// not silently raised to `1`.
/// `end` is the absolute end of the LENDING, in unix seconds, and it
/// is the fourth clamp: a grant whose end has already passed mints nothing at
/// all, and one still ahead is copied onto the lease so the borrower can render
/// "ends in 1 h" without a second round trip.
///
/// It is a PARAMETER rather than read off `granted`
/// ([`crate::peer::config::LendGrant::until`], which is where it is now stored)
/// so this function stays pure and testable at any instant: the clamp being
/// decided is "is this end ahead of `now_ms`", and a function that dug the end
/// out of the grant itself could not be asked about an end the grant does not
/// carry. [`Ledger::grant`] is the one caller that passes `grant.until`.
///
/// `schedule_close_ms` is the FIFTH clamp, on [`Lease::expires_at_ms`] rather
/// than on whether a lease mints at all: `--between`/`--days` is consulted
/// once, here, by [`Ledger::grant`], and a lease minted a minute before the
/// window closes used to keep its full TTL, so it went on serving hours past
/// the hours it was lent for because nothing after mint ever asked the
/// schedule again. `expires_at_ms` is already documented as the renewal TTL a
/// borrower re-asks at, never lowered below what the schedule allows, so
/// clamping it here means the re-ask lands back in [`Ledger::grant`], which
/// refuses `OutsideSchedule` honestly once the window is shut, the same
/// answer a borrower asking for the first time after close already gets. A
/// `None` here is "this grant has no schedule, or the mint is already
/// refused for a different reason", never "the window is always open"; the
/// caller passes `None` in both cases and this function does not tell them
/// apart, because neither one clamps.
#[expect(
    clippy::too_many_arguments,
    reason = "the ask, the lender's grant and the lease's clock: inputs from three places"
)]
pub fn clamp_to_grant(
    ask: &LeaseRequest,
    granted: Option<LendGrant>,
    inspect: bool,
    lendable: f64,
    now_ms: i64,
    lease_id: u128,
    end: Option<u64>,
    schedule_close_ms: Option<i64>,
) -> LeaseGrant {
    let refuse = |refusal: LeaseRefusal| LeaseGrant {
        lease: None,
        refusal: Some(refusal),
    };
    if !inspect {
        return refuse(LeaseRefusal::InspectNotGranted);
    }
    if ask.window == Window::Unknown {
        return refuse(LeaseRefusal::Unsupported);
    }
    let Some(wanted) = fraction_budget(ask.unit) else {
        return refuse(LeaseRefusal::Unsupported);
    };
    // No grant for this window is not "zero of it": this peer was never told it
    // could borrow this window at all.
    let Some(grant) = granted.filter(|grant| grant.window == ask.window) else {
        return refuse(LeaseRefusal::InspectNotGranted);
    };
    let amount = wanted.min(grant.fraction).min(lendable);
    // A grant below MIN_DEBIT is not a small lease, it is a lease the first
    // request overdraws. `OwnerGuard` rather than `LeaseSpent`, because nothing
    // has been spent: the owner's own headroom is what is missing.
    if amount < MIN_DEBIT || amount.is_nan() {
        return refuse(LeaseRefusal::OwnerGuard);
    }
    // The end, checked here rather than left for `may_relay`: a lease
    // minted past its own end is a lease whose first relay is refused, and
    // refusing to mint it is the same answer told one round trip earlier. The
    // comparison is in SECONDS because that is the unit `Lease::until` carries
    // and converting the deadline (rather than the clock) would round the
    // operator's 18:00 to something else.
    if end.is_some_and(|end| end <= seconds_of(now_ms)) {
        return refuse(LeaseRefusal::LeaseExpired);
    }
    let ttl_s = ask.ttl_s.min(grant.ttl_s);
    let expires_at_ms = now_ms + i64::from(ttl_s) * 1_000;
    // THE FIFTH CLAMP. See this function's own doc: a schedule is consulted
    // at mint only, so the TTL is never allowed to outlive the window it was
    // minted inside.
    let expires_at_ms = match schedule_close_ms {
        Some(close_ms) => expires_at_ms.min(close_ms),
        None => expires_at_ms,
    };
    LeaseGrant {
        lease: Some(Lease {
            lease_id,
            window: ask.window,
            unit: LeaseUnit::Fraction(amount),
            granted_at_ms: now_ms,
            expires_at_ms,
            spent: 0.0,
            max_inflight: ask.max_inflight.min(grant.max_inflight),
            until: end,
        }),
        refusal: None,
    }
}

/// Unix milliseconds as unix SECONDS, floored, for a comparison against
/// [`Lease::until`].
///
/// Floored and not rounded, and negative-safe (`div_euclid`), because the one
/// question asked of it is "has this instant passed?", rounding up would
/// answer yes for the last half-second of a lease the operator still owns.
fn seconds_of(now_ms: i64) -> u64 {
    u64::try_from(now_ms.div_euclid(1_000)).unwrap_or(0)
}

/// What is charged when the observed utilization rise is 0.000. See the module
/// docs: a guess, flagged as one, with the measurement that would replace it.
pub const MIN_DEBIT: f64 = 0.002;

/// One line of an account card's "Lent to …", which is the read-only
/// view of a lease **from the account's side**.
///
/// Computed from the lender's OWN grants and nothing else, no wire, no peer
/// asked, no ledger consulted. That is the whole design of the line: an
/// operator looking at an account wants to know what they have promised, which
/// is a fact about their own file, and a figure that needed a reachable peer
/// would render as blank on a sleeping laptop.
///
/// [`Self::peer`] is what a person reads and [`Self::peer_id`] is what a
/// program joins on. The line carried only the label, which is the operator's
/// own display string: two Macs may be labelled the same, a label is renamed
/// whenever its owner feels like it, and every other cross-surface join in this
/// tree moved to the wire id ([`crate::status::PeerStatusRow::id`]), so a panel
/// merging this line with a peer row had nothing to merge on.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LentTo {
    /// The trusted Mac's label, already sanitized in the peers file. For the
    /// screen; [`Self::peer_id`] is what a row is joined on.
    pub peer: String,
    /// The same Mac's pinned key in its WIRE form, the spelling
    /// `tcr peer ls --json` writes as a row's `node` and the only one
    /// [`tcr_peer_wire::PeerId::parse`] reads back.
    pub peer_id: String,
    /// The lease id, as hex, the handle `tcr peer lend --revoke` takes, so the
    /// panel's click target on this line can revoke or re-lend exactly the
    /// lease the operator is looking at. A string and not a number: see
    /// [`crate::peer::config::LendGrant::id`].
    pub id: String,
    /// What the lease draws from, in the CLI's own `--scope` spelling.
    pub scope: LendScope,
    pub window: Window,
    /// The grant's ceiling, which is what the card's "20 %" is.
    pub fraction: f64,
    /// The end, when the operator set one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until: Option<u64>,
    /// Whether that end has passed. Always present, because an absent key
    /// would read as "still running", the one direction that matters here.
    pub ended: bool,
}

/// Which accounts each of this node's grants lends from, keyed by account
/// label: the `lentTo` block of `tcr peer ls --json`.
///
/// `accounts` is `(name, groups, provider)` per local account, the name `tcr
/// status` prints for it, the groups it is in, and whose account it is. Passed
/// in rather than read off a `Manager` for the reason every other pure function
/// in this file is: the decision being made is scope membership, and it is
/// testable without a fleet.
///
/// **A Codex account is never in the map, under any scope.** No lease draws on
/// one: `Manager::lendable_fraction` and the serve path both skip every account
/// that is not a Claude account, because lending a Codex account over the mesh
/// is not something this version does. A Codex card reading "Lent to …" would
/// promise a lease that spends nothing of it, so a Codex account is skipped
/// before the scope is even asked about, whatever the scope says.
///
/// **The map is keyed by that name, and membership is decided on the SANITIZED
/// label.** The two are the same string for every account whose name is
/// already a label, and they part only for one whose is not, an email, a
/// uuid. That account is still in the map (its card shows the line under an
/// `all` or a `group:` lease, which was fixed) and still cannot
/// be picked out by `account:<label>`, because
/// [`tcr_peer_wire::sanitize_label`] refuses the name and
/// [`tcr_peer_wire::LendScope::parse`] refuses to build a scope naming it. One
/// vocabulary for the key (the panel joins its account card on it), one for
/// the match ([`crate::manager::Manager::lendable_fraction`] uses exactly the
/// same sanitized value, so the headroom and the line agree about which
/// accounts a scope draws from).
///
/// An account no grant reaches is **absent from the map, not present with an
/// empty list**: the panel hides the line when it is empty, and
/// "no key" is one fact for it to read rather than two.
///
/// # An ENDED lease is still a row
///
/// The rule is: "a lease with an end that passed is kept in the list, greyed,
/// so the operator sees what was lent and can re-lend with one click". So a
/// grant past its `until` appears here with [`LentTo::ended`] set rather than
/// being filtered out, the panel greys it, and an operator who cannot see what
/// they lent cannot re-lend it.
pub fn lent_to(
    store: &PeerStore,
    accounts: &[(String, Vec<String>, crate::provider::Provider)],
) -> std::collections::BTreeMap<String, Vec<LentTo>> {
    store.reload_if_changed();
    let mut out: std::collections::BTreeMap<String, Vec<LentTo>> =
        std::collections::BTreeMap::new();
    for row in store.peers() {
        for grant in &row.lend {
            for (name, groups, provider) in accounts {
                // No lease draws on a Codex account, so none is lent to anyone,
                // whatever the scope says. See this function's doc.
                match provider {
                    crate::provider::Provider::Anthropic => {}
                    crate::provider::Provider::Codex => continue,
                }
                // The sanitized label decides membership, the name is the key.
                // See this function's doc. `unwrap_or_default` gives the
                // empty string for a name that is not a label, which
                // `sanitize_label` refuses to produce and so no scope can
                // name: exactly what `Manager::lendable_fraction` does with
                // the same account.
                let label = tcr_peer_wire::sanitize_label(name).unwrap_or_default();
                if !scope_covers(&grant.scope, &label, groups) {
                    continue;
                }
                out.entry(name.clone()).or_default().push(LentTo {
                    peer: row.label.clone(),
                    peer_id: row.node.to_wire(),
                    id: crate::peer::config::lease_id_string(grant.id),
                    scope: grant.scope.clone(),
                    window: grant.window,
                    fraction: grant.fraction,
                    until: grant.until,
                    // `PeerStore::peers` derived this against the clock; it is
                    // never read off the file. See `LendGrant::ended`.
                    ended: grant.ended,
                });
            }
        }
    }
    out
}

/// Whether one lease scope draws from one account.
///
/// The ONE answer to "is this account inside that scope", read by the headroom
/// arithmetic ([`crate::manager::Manager::lendable_fraction`]), by the picker
/// restriction and by [`lent_to`]. Three readers and one function on purpose:
/// a scope that means one set of accounts when the headroom is computed and
/// another when the request is served is a lease that advertises one account's
/// room and spends another's.
///
/// A group is matched by NAME against the account's own group list, so the
/// answer follows `tcr group` hot-reloads rather than a membership frozen at
/// lend time.
pub fn scope_covers(scope: &LendScope, label: &str, groups: &[String]) -> bool {
    match scope {
        LendScope::All => true,
        LendScope::Group(name) => groups.iter().any(|group| group == name),
        LendScope::Accounts(labels) => labels.iter().any(|named| named == label),
    }
}

/// Every lender row in a peers file, for the provider and for the surfaces that
/// count them.
pub fn lender_rows(store: &PeerStore) -> Vec<PeerRow> {
    store
        .peers()
        .into_iter()
        .filter(|row| row.allow.allow_disclose && row.has_endpoint())
        .collect()
}
