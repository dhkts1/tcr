//! The lender's ledger: every lease it granted, its persistence, and the caches that bound it.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;

use anyhow::Result;
use tcr_peer_wire::{Lease, LeaseRefusal, LeaseRequest, LeaseUnit, LendScope, PeerId, Window};

use super::{
    Granted, MIN_DEBIT, RelayRefusal, clamp_to_grant, fraction_budget, schedule_closes_at_ms,
    schedule_refusal, seconds_of,
};
use crate::peer::config::{LendGrant, PeerStore};
use crate::peer::id::random_id;
use crate::peer::serve;

/// How many `(lease_id, request_id)` pairs one ledger remembers.
///
/// The same figure [`crate::peer::listener::REQUEST_DEDUP_CAPACITY`] uses, and
/// named through it rather than re-typed: the two caches answer the same
/// question at two layers (the stream gate per connection, this one per
/// process) and two spellings of one bound is how they come to disagree about
/// what a burst is.
pub const SERVED_CAPACITY: usize = crate::peer::listener::REQUEST_DEDUP_CAPACITY;

/// How long a served pair is remembered. Same figure, same reason.
pub const SERVED_TTL_MS: i64 = crate::peer::listener::REQUEST_DEDUP_TTL_MS;

/// How many request ids are remembered **per lease**.
///
/// [`SERVED_CAPACITY`] is the whole process's bound and it used to be the only
/// one: one FIFO for every lease, so a borrower sending 4 096 requests of its
/// own evicted every other borrower's pairs, and the replay defence for those
/// leases went quiet. Worse, it could evict its OWN older pair and then replay
/// it for free.
///
/// So the cache is per lease and the eviction is per lease: a busy borrower
/// can only ever push its own oldest id out, which is the one direction that
/// costs nobody else anything. The process-wide bound is
/// [`SERVED_CAPACITY`] still, applied across leases by
/// [`Ledger::admit_pair`], so the memory is bounded by the same figure it
/// always was.
pub const SERVED_PER_LEASE_CAPACITY: usize = 256;

/// How many LIVE leases one pinned peer may hold here at once.
///
/// The review's MEDIUM: nothing capped how many leases one peer could mint, and
/// every mint rewrites the whole state file under the same [`FileLock`] that
/// `tcr peer accept` and `tcr peer block` take with a hard five-second
/// give-up, so an authenticated peer asking in a loop was both an unbounded
/// ledger and an operator whose Accept could not get the lock.
///
/// Eight, and per GRANTEE rather than per Mac, because the honest question is
/// "how many leases does one borrower legitimately need at once" and the answer
/// is a small number: a lease is asked for per window, held for its ttl and
/// renewed rather than re-minted ([`PeerLeaseProvider::lease_for`](super::PeerLeaseProvider::lease_for) re-asks only
/// when its cached one has expired or the window changed). Two windows and a
/// couple of retries in flight is inside it; a loop is not.
///
/// A cap and never a queue: the (n+1)th is REFUSED, the same decision
/// [`Lease::max_inflight`] makes one layer down and for the same reason, a
/// borrower told no can serve the request locally now, and a borrower made to
/// wait learns nothing and still holds the client.
///
/// # The wire has its own word for this refusal
///
/// It is answered as [`LeaseRefusal::TooManyLeases`], not
/// [`LeaseRefusal::OwnerGuard`]: the lease is refused by how many this peer
/// already holds, not by anything about the owner's headroom or the ask, and
/// a borrower that cannot tell the two apart either hammers a dead lease or
/// backs off from a live one for the wrong reason.
pub const MAX_LEASES_PER_PEER: usize = 8;

/// One lease scope's canonical key in [`Ledger::headroom`].
///
/// [`LendScope`]'s own `Display` is the CLI's `--scope` spelling
/// (`all`, `group:<name>`, `account:<a>,<b>`), which is already canonical,
/// one string per distinct scope, and the string an operator typed. A separate
/// hash derive on the wire type would be a second answer to "are these the same
/// scope".
fn scope_key(scope: &LendScope) -> String {
    scope.to_string()
}

/// Could a lease on `held` be spending accounts a lease on `wanted` would also
/// draw from?
///
/// # It answers with what the LEDGER knows, and errs towards subtracting
///
/// This ledger holds scopes as the operator wrote them and no membership at
/// all: groups hot-reload, which is the whole reason [`LendScope::Group`] is a
/// name and not a frozen list, so "does `group:work` contain account `a`" is a
/// question only the manager can answer and only at the instant it is asked.
///
/// So three answers are derivable here and the fourth is a choice.
/// [`LendScope::All`] overlaps everything. Two [`LendScope::Accounts`] sets
/// overlap when they name an account in common. Identical scopes overlap.
/// Anything involving a GROUP against a different scope is not derivable, and
/// it answers "overlapping": the cost of subtracting room that is really
/// somebody else's is a lease smaller than it could have been, and the cost of
/// not subtracting is the owner promising the same headroom twice, which is the
/// failure this function exists for.
fn scopes_overlap(wanted: &LendScope, held: &LendScope) -> bool {
    match (wanted, held) {
        (LendScope::All, _) | (_, LendScope::All) => true,
        (LendScope::Accounts(wanted), LendScope::Accounts(held)) => {
            wanted.iter().any(|label| held.contains(label))
        }
        // A group against anything but an identical group: not derivable here,
        // so it counts. See this function's own doc.
        _ => true,
    }
}

/// One relayed request this ledger remembers, and everything it knows about it.
///
/// The `charged` flag is why this is a struct and not the `(request_id, at_ms)`
/// tuple it replaced: the serve half and the charge half used to keep separate
/// maps of separate pairs, and a pair could be forgotten by one while the other
/// still held it. See [`Ledger::pairs`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RelayedPair {
    request_id: u128,
    /// The millisecond this pair was admitted for relay, on the clock
    /// [`Ledger::enter_relay`]'s caller passed. Every entry here is stamped by
    /// that one caller, so [`Ledger::admit_pair`]'s expiry and eviction compare
    /// readings of the same clock.
    at_ms: i64,
    /// Whether [`Ledger::debit`] has already charged this pair.
    charged: bool,
}

/// The lender's ledger: every lease it has granted, with absolute deadlines.
///
/// Persisted and restored at boot, TTL-bounded, and it logs how many rows it
/// restored and how many it dropped, the same shape session affinity already
/// uses, because "restored=N expired=M" is the line that tells an operator
/// whether a restart cost them anything.
#[derive(Debug, Clone, Default)]
pub struct Ledger {
    leases: Vec<Lease>,
    /// Where [`Self::persist`] writes the rows [`Self::restored_from`] reads
    /// back, or `None` for a ledger that is process state only (every test that
    /// builds one with [`Self::new`], and any build with no peers file).
    ///
    /// The review's M2: this type's own doc promised "Persisted and restored at
    /// boot … restored=N expired=M" and nothing serialized it, while
    /// [`crate::peer::state::PeerState::leases`] declared the slot and was
    /// written by nobody. A restart voided every lease. The promise is kept
    /// rather than deleted, because a restart mid-lease otherwise strands a
    /// borrower whose ceiling the lender has forgotten, and an unbounded
    /// re-grant after a restart is the same hole H2 opened, one reboot wide.
    state_path: Option<PathBuf>,
    /// Who each lease was granted TO, on the lender and only on the lender.
    ///
    /// The review's M1. See [`RelayRefusal::NotTheGrantee`]. A side map rather
    /// than a field on [`Lease`] for the same reason [`Self::scopes`] is one:
    /// `Lease` is the wire message, and a grantee on it would tell the borrower
    /// its own id back while adding a field the wire message does not need.
    grantees: HashMap<u128, PeerId>,
    /// The owner's own remaining headroom per window, as last measured by
    /// [`crate::manager::Manager::lendable_fraction`].
    ///
    /// **This field is why [`Ledger::may_relay`] can answer
    /// [`LeaseRefusal::OwnerGuard`] at all.** The skeleton's signature takes a
    /// lease id and a clock and nothing else, so the guard has to already be in
    /// the ledger when the question is asked; the lender writes it through
    /// [`Ledger::note_owner_headroom`] whenever it re-reads its own quota. An
    /// unmeasured window is **absent, not zero-headroom and not
    /// infinite-headroom**, and [`Ledger::may_relay`] refuses on absence: giving
    /// away quota on a window nobody has measured is the one direction where
    /// guessing costs the owner rather than the borrower.
    ///
    /// # Keyed by SCOPE and window, not by window alone
    ///
    /// The fraction is of the SCOPE's
    /// headroom, and this map was keyed by window, so a lease scoped to one
    /// group was clamped by (and its owner guard decided against) the whole
    /// fleet's headroom. A group with no room left then went on lending the
    /// pool's.
    ///
    /// The key is [`scope_key`]'s canonical spelling of the scope rather than
    /// the [`LendScope`] itself, because that type is in `tcr_peer_wire` and
    /// does not derive `Hash`, and nothing here adds the derive there. Both
    /// writers ([`Self::grant`] and
    /// [`crate::peer::serve::handle_serve_on`]) note the scope's own figure
    /// through [`crate::peer::serve::WindowUtilization::lendable`] immediately
    /// before they ask, so the measurement a relay is decided against is the
    /// scope's and is seconds old.
    headroom: HashMap<(String, Window), f64>,
    /// Relayed requests currently in flight, per lease, against
    /// [`Lease::max_inflight`]. See [`Ledger::enter_relay`].
    ///
    /// A key at zero is REMOVED rather than left behind (the review's M4): a
    /// map a peer can grow one entry per lease-ask is a memory bug with a
    /// security label, which is the bar `RefusalLog` and `RequestDedup` next
    /// door already hold themselves to.
    inflight: HashMap<u128, u8>,
    /// Every `(lease_id, request_id)` pair this ledger remembers, newest last,
    /// each carrying whether the charge for it has landed yet.
    ///
    /// Three defects in one field. The review's **H2**: the memory was the
    /// charge's alone, consulted only by [`Self::debit`], so a replayed
    /// `request_id` was served again for free; [`Self::enter_relay`] now
    /// refuses the pair, so the ceiling binds. The review's **M4**: it was an
    /// unbounded `HashSet` that grew one entry per relayed request for the life
    /// of the process, on input a peer chooses. Bounded by [`SERVED_CAPACITY`]
    /// and [`SERVED_TTL_MS`], exactly the way
    /// [`crate::peer::listener::RequestDedup`] already is, the `VecDeque` is
    /// what makes "drop the oldest" possible at all. **Keyed by lease**, the
    /// second half of the M4 fix: one FIFO for the whole process meant a
    /// borrower sending its own [`SERVED_CAPACITY`] requests evicted every
    /// other borrower's pairs, and its own older ones, which it could then
    /// replay for free. See [`SERVED_PER_LEASE_CAPACITY`].
    ///
    /// **And it is ONE map rather than two.** "Served here" and "charged here"
    /// lived in separate maps, filled by different callers on different clocks:
    /// every admitted relay entered the first, only the ones that reached the
    /// charge entered the second, and the two FIFOs therefore evicted different
    /// pairs at different moments. A pair that had left the serve map while
    /// still sitting in the charge map was admitted again (new to one) and
    /// charged nothing (old to the other), which is a relay bought on the
    /// lender's own account for free. One map with a state per pair makes
    /// forgetting a pair forget both facts about it at once, so the two answers
    /// cannot disagree.
    pairs: HashMap<u128, VecDeque<RelayedPair>>,
    /// What each lease draws from, on the LENDER and only on the lender.
    ///
    /// A side map rather than a field on [`Lease`], because `Lease` is the wire
    /// message, and a scope never crosses it: the
    /// borrower never learns which of the lender's accounts paid. The gate that
    /// holds that is `every_wire_type_serializes_only_allowlisted_keys` in
    /// `tests/peer_wire.rs`, not this comment.
    ///
    /// A lease with no entry here is [`LendScope::All`], which is what every
    /// lease minted before scopes existed meant, including the ones a restore at
    /// boot reads back off a ledger file written by an older build.
    scopes: HashMap<u128, LendScope>,
    /// A `spent` figure that has moved since the last write, waiting for
    /// [`Self::flush`].
    ///
    /// **Why the write is not where the charge is.** [`Self::persist`] takes
    /// [`crate::peer::config::FileLock`], loads the whole state file and saves
    /// it, and [`Self::debit`] is called from
    /// [`crate::peer::serve::handle_serve_on`] with this ledger's own mutex
    /// held, on every single relayed request. So the relay path paid a locked
    /// file round trip per request, and a lockfile held by a concurrent
    /// `tcr peer accept` made every relay on this Mac wait out
    /// [`crate::peer::config::LOCK_STALE_MS`] with the ledger mutex in hand:
    /// one slow operator verb stalled the whole lender.
    ///
    /// A grant still writes immediately ([`Self::record_scoped`]): it is rare,
    /// it is the row a restart cannot reconstruct, and nothing is in flight
    /// against it yet. A charge only marks this flag, and the lender's
    /// debounced flusher ([`crate::peer::listener::LEDGER_FLUSH_INTERVAL`])
    /// does the write off the hot path. The cost of a crash inside one
    /// debounce window is bounded by that interval and is `spent` moving
    /// backwards by at most one window's charges, which `may_relay` still
    /// bounds by the lease's own budget and TTL.
    dirty: bool,
    /// The path each lease's borrows are served over, when the serving side
    /// knew one.
    ///
    /// A side map for [`Self::grantees`]' reason and one more: a path is not a
    /// property of the lease at all, it is a property of the STREAM that spends
    /// it, and a lease whose borrower moves house keeps its id and changes its
    /// path. The newest note wins, which is what makes the figure it feeds
    /// ([`crate::peer::tunnel::PathMeter`]) a measurement of this hour rather
    /// than of the hour the lease was minted in.
    ///
    /// A lease with no entry is a borrow whose path this build could not
    /// attribute, and its tokens are charged to no path at all, never to a
    /// guessed one. Nothing notes it from the serving stream yet;
    /// [`Self::note_lease_path`] is this side of it.
    lease_paths: HashMap<u128, crate::peer::config::Locator>,
}

impl Ledger {
    /// An empty ledger with no file behind it: process state only.
    ///
    /// [`Self::restored_from`] is what a lender boots with. This stays for the
    /// tests and for a build with no peers file, and it is now honest about
    /// what it is rather than promising a restore that never happened.
    pub fn new() -> Self {
        Self::default()
    }

    /// The ledger as of the last write, read back off `state_path`, with the
    /// `restored=N expired=M` line this type's doc has always promised.
    ///
    /// A state file that cannot be read is an EMPTY ledger plus a warning, and
    /// never a refusal to boot: the recovery is already designed and already
    /// gentle ([`Self::may_relay`] answers `LeaseExpired` for an id it does not
    /// hold, `open_serve` turns that into `Ok(None)`, and the borrower drops its
    /// cached hint and re-asks). A lender that refused to serve local traffic
    /// over a stale runtime-state file would be the worse trade.
    ///
    /// Expired rows are counted and DROPPED here rather than recorded and left
    /// to [`Self::may_relay`], which is the opposite of what [`Self::record`]
    /// does on purpose: `record` is the wire path, where a silent drop would put
    /// the count and the ledger out of step, and this is the boot path, where
    /// the count IS the log line and carrying dead rows forward would grow the
    /// file by every lease the machine ever granted.
    pub fn restored_from(state_path: &std::path::Path) -> Self {
        let mut ledger = Self {
            state_path: Some(state_path.to_path_buf()),
            ..Self::default()
        };
        let now_ms = crate::now_ms();
        let rows = match crate::peer::state::load(state_path, now_ms) {
            Ok(state) => state.leases,
            Err(err) => {
                tracing::warn!(
                    path = %state_path.display(),
                    error = %err,
                    "peer lease: the ledger's state file could not be read, so this lender \
                     starts with no leases (every borrower re-asks)"
                );
                return ledger;
            }
        };
        let mut expired = 0_usize;
        let mut ungranted = 0_usize;
        for row in rows {
            if row.lease.expires_at_ms <= now_ms {
                expired += 1;
                continue;
            }
            // THE OTHER HALF OF `Self::persist`'s rule. No build writes an
            // all-zero grantee any more, but an older one did and the file is
            // hand-editable JSON, so the reader refuses it rather than trusting
            // that nothing ever wrote it: a lease nobody can spend
            // (`enter_relay` answers `NotTheGrantee` to every peer, including
            // one whose id really is all-zero, because the all-zero id is not a
            // static key any handshake can produce) would still count in `live`
            // and `committed_fraction` and hold headroom for its whole TTL.
            if row.peer == PeerId([0_u8; 32]) {
                ungranted += 1;
                continue;
            }
            ledger.grantees.insert(row.lease.lease_id, row.peer);
            ledger.scopes.insert(row.lease.lease_id, row.scope);
            ledger.leases.push(row.lease);
        }
        tracing::info!(
            path = %state_path.display(),
            restored = ledger.leases.len(),
            expired,
            ungranted,
            "peer lease: ledger restored"
        );
        ledger
    }

    /// Write every live lease back to the state file, or do nothing for a
    /// ledger with no file behind it.
    ///
    /// Called from the two places that CHANGE what a restart would have to know:
    /// [`Self::record_scoped`] (a new lease exists) and [`Self::debit`] (a live
    /// lease's `spent` moved, which is the ceiling H2 exists to keep binding).
    /// Not from `enter_relay`/`leave_relay`: in-flight is concurrency state, and
    /// a restart releases every slot by construction.
    ///
    /// A failed write is LOGGED and never propagated. The caller is answering a
    /// borrower on a live stream, and a lender that refused a request it can
    /// serve because a cache file is read-only would be trading a real service
    /// for a durability promise about a restart that may never happen. The
    /// warning is how an operator learns their leases will not survive one.
    /// Write the charges [`Self::debit`] has recorded since the last write, and
    /// report whether there was anything to write.
    ///
    /// The lender's debounced flusher
    /// ([`crate::peer::listener::flush_ledger_periodically`]) is the caller,
    /// plus shutdown. Idempotent and cheap when nothing moved: a lender that
    /// served no relayed request takes no lock and touches no file.
    pub fn flush(&mut self) -> bool {
        if !self.dirty {
            return false;
        }
        self.persist();
        self.dirty = false;
        true
    }

    /// Whether a charge is waiting for [`Self::flush`], so a gate reads the
    /// hot path's promise off the ledger rather than off a copy.
    pub const fn is_dirty(&self) -> bool {
        self.dirty
    }

    fn persist(&self) {
        let Some(path) = &self.state_path else {
            return;
        };
        // A LEASE WITH NO GRANTEE IS NOT WRITTEN. `enter_relay` refuses every
        // request against one (`RelayRefusal::NotTheGrantee`), so such a row
        // can never be spent, but it is not inert either: `live` and
        // `committed_fraction` count it, so restoring one holds a slice of this
        // lender's headroom against a lease nobody on earth can use, until its
        // own TTL runs out. This path used to write the all-zero id as "the
        // honest round-trip of nobody"; a row that costs headroom and serves
        // nobody is the one thing a restart is better off forgetting. Every
        // lease `grant` mints is recorded through `record_scoped` with the
        // grantee from its authenticated session, so this drops nothing an
        // operator asked for; a `record`-only caller (the older entry point)
        // is what it catches.
        let mut ungranted = 0_usize;
        let rows: Vec<crate::peer::state::LeaseRow> = self
            .leases
            .iter()
            .filter_map(|lease| {
                let Some(peer) = self.grantees.get(&lease.lease_id).copied() else {
                    ungranted += 1;
                    return None;
                };
                Some(crate::peer::state::LeaseRow {
                    lease: *lease,
                    peer,
                    scope: self.scope_of(lease.lease_id),
                })
            })
            .collect();
        if ungranted > 0 {
            tracing::warn!(
                path = %path.display(),
                ungranted,
                persisted = rows.len(),
                "peer lease: leases with no grantee are not persisted; nothing can spend one \
                 and restoring it would hold this lender's headroom for its whole TTL"
            );
        }
        if let Err(err) = crate::peer::state::save_leases(path, &rows) {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "peer lease: the ledger could not be persisted, so a restart voids these \
                 leases and every borrower re-asks"
            );
        }
    }

    /// Put one lease in the ledger: the single insertion point, used by
    /// [`Self::grant`] when it mints one and by the restore-at-boot path when it
    /// reads one back off disk.
    ///
    /// An ALREADY EXPIRED lease handed to THIS function is accepted rather than
    /// filtered, because [`Self::may_relay`] refuses one anyway and a caller
    /// that wrote it deserves to read it back. The boot path does its own
    /// counting and dropping ([`Self::restored_from`]) precisely so the
    /// "restored=N expired=M" line and the ledger cannot disagree: one place
    /// counts, one place accepts, and neither silently does the other's job.
    pub fn record(&mut self, lease: Lease) {
        // The review's M4: `leases` was pushed to and never pruned, and
        // `may_relay`/`debit` each walk it linearly, so a peer that asks for a
        // lease per second grows both the memory and the cost of every later
        // question. An expired row can answer nothing but `LeaseExpired`, which
        // is the same answer an absent one gives, so dropping it here loses no
        // fact, and it happens on the ONE insertion point, so there is no path
        // that grows the vector without paying for the prune.
        let now_ms = crate::now_ms();
        self.leases.retain(|held| held.expires_at_ms > now_ms);
        let live: HashSet<u128> = self.leases.iter().map(|held| held.lease_id).collect();
        self.grantees.retain(|id, _| live.contains(id));
        self.scopes.retain(|id, _| live.contains(id));
        self.inflight.retain(|id, _| live.contains(id));
        self.leases.push(lease);
    }

    /// [`Self::record`], plus what the lease draws from.
    ///
    /// Two entry points rather than one with an `Option`, because the restore
    /// path and the older callers genuinely mean [`LendScope::All`] and saying
    /// so by omission is how a scoped lease comes to be restored as unscoped.
    /// [`Self::scope_of`] answers `All` for anything recorded the other way.
    ///
    /// `peer` is the GRANTEE and it is not optional: the review's M1 was that a
    /// lease remembered nothing about who asked, so the id alone spent it. A
    /// lease recorded here can be spent by this peer and by nobody else
    /// ([`RelayRefusal::NotTheGrantee`]), and the pair is what
    /// [`Self::persist`] writes so a restart does not turn every lease back
    /// into a bearer token.
    pub fn record_scoped(&mut self, lease: Lease, peer: PeerId, scope: LendScope) {
        self.record(lease);
        // AFTER `record`, which prunes the maps down to the live lease set: the
        // other order dropped the row this call is about.
        self.grantees.insert(lease.lease_id, peer);
        self.scopes.insert(lease.lease_id, scope);
        // A GRANT writes now. It is rare, it is the row a restart cannot
        // reconstruct from anywhere else, and no request is in flight against
        // it yet, the opposite of `debit` on all three counts, which is why
        // that one only marks `Self::dirty`.
        self.persist();
        self.dirty = false;
    }

    /// Who one lease was granted to, or `None` for a lease this ledger does not
    /// hold. For `tcr peer ls --json` and the panel row.
    pub fn grantee_of(&self, lease_id: u128) -> Option<PeerId> {
        self.grantees.get(&lease_id).copied()
    }

    /// What one lease draws from, on the lender. [`LendScope::All`] for a lease
    /// recorded without one. See [`Self::scopes`].
    pub fn scope_of(&self, lease_id: u128) -> LendScope {
        self.scopes
            .get(&lease_id)
            .cloned()
            .unwrap_or(LendScope::All)
    }

    /// Tell the ledger what the owner's own guard band leaves on one window.
    ///
    /// `fraction` is [`crate::manager::Manager::lendable_fraction`]'s answer:
    /// headroom ABOVE the guard band, never the raw threshold, and `0.0` is the
    /// common answer. Called by the lender whenever it re-reads its own quota,
    /// so [`Self::may_relay`] reads a measurement rather than taking one.
    /// `LendScope::All`'s figure, which is what a lender's boot ticker measures
    /// for the whole fleet. [`Self::note_scope_headroom`] is the scoped form.
    pub fn note_owner_headroom(&mut self, window: Window, fraction: f64) {
        self.note_scope_headroom(&LendScope::All, window, fraction);
    }

    /// Tell the ledger what one SCOPE's accounts leave on one window.
    ///
    /// The rule is: "the fraction is of the SCOPE's headroom … computed by
    /// `lendable_fraction` over the scope's accounts only". Both readers of a
    /// scoped lease note the scope's own figure immediately before they ask,
    /// [`Self::grant`] when it mints, `handle_serve_on` when it relays, so the
    /// guard a scoped relay is decided against is the scope's and not the
    /// fleet's. See [`Self::headroom`].
    pub fn note_scope_headroom(&mut self, scope: &LendScope, window: Window, fraction: f64) {
        self.headroom
            .insert((scope_key(scope), window), fraction.max(0.0));
    }

    /// What this ledger last measured for one scope on one window.
    ///
    /// **`LendScope::All`'s figure is the fallback and it is a CEILING, not a
    /// substitute**: `min` of the two, so a scope with no note of its own can
    /// never be told it has more room than the whole fleet has. That direction
    /// is the decision, the other one (trust the scope's own note alone) would
    /// let a stale per-scope figure outlive the fleet running dry, and refusing
    /// outright on an absent scope note would refuse every lease minted by a
    /// build or a test whose reader cannot answer per scope
    /// ([`crate::peer::serve::WindowUtilization::lendable`] defaults to `None`).
    ///
    /// `None` for a window nothing has measured at all, which
    /// [`Self::may_relay`] refuses, because giving away quota on an unmeasured
    /// window is the one direction where guessing costs the owner.
    fn headroom_for(&self, scope: &LendScope, window: Window) -> Option<f64> {
        let all = self
            .headroom
            .get(&(scope_key(&LendScope::All), window))
            .copied();
        let scoped = self.headroom.get(&(scope_key(scope), window)).copied();
        match (scoped, all) {
            (Some(scoped), Some(all)) => Some(scoped.min(all)),
            (Some(scoped), None) => Some(scoped),
            (None, all) => all,
        }
    }

    /// Answer a borrower's request. **The answer is the offer**: there is no
    /// negotiation round, because a fraction changes on every request and a
    /// second round trip would be spent on a stale number.
    ///
    /// Reads the grant for that peer, then what the manager says is lendable
    /// right now ([`crate::manager::Manager::lendable_fraction`]), and never
    /// the raw threshold.
    /// # The arithmetic is not here, and that is the point
    ///
    /// Everything this function DECIDES is in [`clamp_to_grant`], which is pure
    /// and tested without a peers file or a random source. What is left is two
    /// reads (the peer's row, the owner's last measured headroom), a lease id
    /// from the node's CSPRNG ([`random_id`]) and a `record`. None of the
    /// lease's arithmetic is hiding in it.
    ///
    /// The peer's row is re-read through `PeerStore::reload_if_changed`, so a
    /// grant revoked a second ago is refused without a restart, which is the
    /// whole reason peer policy lives in its own hot-reloaded file.
    ///
    /// `fleet` is the lender's own fleet, and it is here for one question:
    /// which of this peer's grants this Mac's picker could actually hold a
    /// request inside ([`crate::peer::serve::WindowUtilization::scope_restriction`]).
    /// The SAME reader the serving leg uses, so a lease this mints is one the
    /// serve path will not turn round and refuse. See
    /// [`crate::peer::config::PeerRow::grant_for`].
    pub fn grant(
        &mut self,
        peer: &PeerId,
        ask: &LeaseRequest,
        store: &PeerStore,
        fleet: &dyn serve::WindowUtilization,
    ) -> Granted {
        store.reload_if_changed();
        // THE CAP, before the row is even looked at: this refusal is about how
        // many leases this peer already holds and nothing about the ask, so
        // asking it first is what keeps a peer in a mint loop from costing a
        // peers-file read and a state-file rewrite per ask. See
        // [`MAX_LEASES_PER_PEER`].
        let now_ms = crate::now_ms();
        let held = self.live_for(peer, now_ms);
        if held >= MAX_LEASES_PER_PEER {
            tracing::warn!(
                peer = %peer.display(),
                held,
                cap = MAX_LEASES_PER_PEER,
                window = ?ask.window,
                "peer lease: this peer already holds the most leases one peer may hold here, \
                 so nothing is granted until one of them expires or is spent"
            );
            return Granted::unfunded(LeaseRefusal::TooManyLeases);
        }
        let row = store.row(peer);
        let inspect = row.as_ref().is_some_and(|row| row.allow.inspect);
        // A peer may hold SEVERAL grants, so this picks the one
        // on the asked-for window that has not ended and whose scope this Mac
        // can serve inside. An ended or unenforceable grant is skipped rather
        // than picked-and-refused: an operator whose `work` lease ran to 18:00
        // and whose `all` lease has no end meant the second one to keep
        // serving, and picking the first row by window alone would refuse the
        // request with a lease sitting right beside it.
        let granted = row
            .as_ref()
            .and_then(|row| {
                row.grant_for(ask.window, seconds_of(crate::now_ms()), &|scope| {
                    fleet.scope_restriction(scope) != serve::ScopeRestriction::Unenforceable
                })
            })
            .cloned();
        // The window, read off the same grant, and asked BEFORE
        // the fleet is measured: a Mac outside its lending hours has nothing to
        // measure for, and a measurement noted here would be spent deciding a
        // request that is already refused.
        //
        // `now_utc()` and not `now_ms`: `Schedule::contains` converts to this
        // Mac's local offset itself, which is the offset an operator means by
        // "between 22:00 and 08:00". A grant with no schedule at all answers
        // `None` from `LendGrant::schedule` and never reaches this branch.
        //
        // The instant itself is kept beside the schedule (`schedule_now`),
        // because `schedule_close_ms` below answers off the SAME reading:
        // asking the wall clock again a few lines down would let a request
        // straddling a clock tick see `contains` say yes against one second
        // and the close computed against a different one.
        let schedule_now = time::OffsetDateTime::now_utc();
        let schedule = granted.as_ref().and_then(LendGrant::schedule);
        if let Some(schedule) = schedule.as_ref()
            && let Some(refusal) = schedule_refusal(Some(schedule), schedule_now)
        {
            tracing::info!(
                peer = %peer.display(),
                window = ?ask.window,
                "peer lease: this grant is outside the hours it was lent for, so nothing \
                 is granted until it opens again"
            );
            return Granted::unfunded(refusal);
        }
        // THE FIFTH CLAMP's input: the window this lease is being minted
        // inside stops being open at this instant, so nothing after mint has
        // to consult the schedule again. See `clamp_to_grant`'s own doc and
        // `schedule_closes_at_ms` below, this module's own helper:
        // `Schedule`'s fields are public and this is the one caller, so the
        // close-time arithmetic lives beside the clamp it feeds rather than
        // adding a method to a module this unit does not own.
        let schedule_close_ms = schedule
            .as_ref()
            .and_then(|schedule| schedule_closes_at_ms(schedule, schedule_now));
        // The scope, read off the grant the operator wrote. It stays
        // HERE: recorded beside the lease in this ledger and never put on the
        // wire.
        let scope = granted
            .as_ref()
            .map_or(LendScope::All, |grant| grant.scope.clone());
        // What the owner's guard band leaves ON THIS SCOPE's accounts, measured
        // NOW through the same fleet reader the serving leg uses. A measurement
        // found this keyed by window alone, which clamped a group-scoped
        // lease by the whole fleet's room. The figure is NOTED as well as used,
        // so `may_relay` decides the first relay against the same measurement
        // rather than against an absence.
        // **Measured HERE, for this scope, at this instant**, and noted, so
        // `may_relay` decides the first relay against the same figure rather
        // than against an absence.
        //
        // Every scope, `LendScope::All` included. The boot ticker
        // (`server.rs`'s `note_owner_headroom`, before the accept loop and every
        // 30 s after) is not a second answer to be avoided: it is the FLOOR, the
        // thing that gives `may_relay` a figure when nobody has asked for a
        // lease yet. A reader that is holding the fleet and about to decide is
        // strictly better placed than a note up to thirty seconds old, measured
        // on the e2e, where a lender warmed one request and the boot note still
        // said `0.0`, so the borrow was refused `owner-guard` for half a minute
        // after the room existed.
        //
        // `None` is not zero: `Manager::lendable` answers it when no account in
        // the scope has a measured window at all, which leaves the ticker's
        // figure standing. See that method's doc for why the distinction has to
        // be made where the quota is.
        if let Some(measured) = fleet.lendable(&scope, ask.window) {
            self.note_scope_headroom(&scope, ask.window, measured);
        }
        // WHAT IS ALREADY PROMISED, SUBTRACTED. `headroom_for` is what this
        // Mac's own fleet has left; it says nothing about what this Mac has
        // already lent out of it. A borrower that re-asks therefore held up to
        // `MAX_LEASES_PER_PEER` leases at a time, each clamped to the FULL
        // fraction, so eight asks promised eight times the room that exists,
        // and every one of them passed `may_relay` until the quota itself ran
        // out. See [`Self::committed_fraction`].
        let lendable = (self.headroom_for(&scope, ask.window).unwrap_or(0.0)
            - self.committed_fraction(&scope, ask.window, now_ms))
        .max(0.0);
        // A HAND GRANT IS CLAMPED AGAIN, by what the accounts whose bearer can
        // leave this Mac have left. The two figures were measured over two
        // different sets of accounts: `handoff_bearer` skips a held account and
        // a strictly pinned one, `lendable_fraction` skipped neither, so a
        // scope whose usable accounts were all held or pinned funded a lease
        // with no bearer at all and every request on it fell back to the
        // lender's own path. It is a clamp rather than a replacement of the
        // note above, because the noted headroom is what `may_relay` decides
        // SERVE relays against too and lowering it there would refuse them for
        // a hand lease's reason.
        //
        // Read off the grant the ledger itself picked, never re-derived: this
        // is the same `granted` the mode, the scope and the end come off.
        let lendable = match granted.as_ref().map(|grant| grant.mode) {
            Some(crate::peer::config::LendMode::Hand) => fleet
                .lendable_by_hand(&scope, ask.window)
                .map_or(lendable, |handable| lendable.min(handable)),
            _ => lendable,
        };

        let lease_id = match random_id() {
            Ok(id) => id,
            Err(err) => {
                // A CSPRNG that will not answer is not a refusal about this
                // lease, and it must not be told as one: a guessable lease id is
                // a lease anybody on the mesh can spend, so the only safe answer
                // is no lease at all, loudly.
                tracing::error!(
                    error = %err,
                    "peer lease: no random source for a lease id, so nothing is granted"
                );
                return Granted::unfunded(LeaseRefusal::Unsupported);
            }
        };

        // The end, read off the grant the operator wrote. A grant
        // whose end has passed mints nothing (`clamp_to_grant`), and one still
        // ahead is copied onto the lease so the borrower can render "ends in
        // 1 h" without a second round trip.
        let end = granted.as_ref().and_then(|grant| grant.until);
        let grant = clamp_to_grant(
            ask,
            granted.clone(),
            inspect,
            lendable,
            crate::now_ms(),
            lease_id,
            end,
            schedule_close_ms,
        );
        if let Some(lease) = grant.lease {
            // The grantee, from the AUTHENTICATED session this answer is for,
            // the review's M1. Nothing about the ask reaches this argument.
            self.record_scoped(lease, *peer, scope.clone());
        }
        Granted {
            answer: grant,
            funding: granted,
            scope,
        }
    }

    /// Enter one relayed request against a lease, or refuse it.
    ///
    /// The whole of [`Self::may_relay`] first, then [`Lease::max_inflight`]. The
    /// (n+1)th concurrent request is **refused, never queued**: a queue in front
    /// of a lease whose debit reads lagging headers just moves the overdraft
    /// later and hides it, and a borrower that is told no can serve the request
    /// locally or answer the honest 429 now instead of after a wait.
    ///
    /// Every `Ok` is paired with exactly one [`Self::leave_relay`]. The caller
    /// holds that pairing in a guard, the way the proxy's own in-flight
    /// accounting does at `src/proxy.rs:2525`, so a rotate, a terminal return or
    /// a panic-unwind all release the slot.
    pub fn enter_relay(
        &mut self,
        lease_id: u128,
        peer: &PeerId,
        request_id: u128,
        now_ms: i64,
    ) -> Result<(), RelayRefusal> {
        // THE GRANTEE, FIRST, and before `may_relay` says anything about the
        // lease: a peer spending somebody else's id must not be able to tell
        // `LeaseSpent` from `LeaseExpired` from `OwnerGuard` about a lease that
        // is not its own. See `RelayRefusal::NotTheGrantee`.
        if self.grantees.get(&lease_id) != Some(peer) {
            return Err(RelayRefusal::NotTheGrantee);
        }
        self.may_relay(lease_id, now_ms)
            .map_err(RelayRefusal::Lease)?;
        // THE REPLAY, and this is the review's H2: the SERVE is made idempotent
        // here, where refusing costs nothing, rather than left to `debit`, where
        // idempotence meant "served again, charged once". Recorded on the way IN,
        // before the request is served, so two concurrent copies of one
        // request id cannot both pass this line.
        if !self.admit_served(lease_id, request_id, now_ms) {
            return Err(RelayRefusal::Replayed);
        }
        // `may_relay` proved the lease is here, so this lookup cannot miss.
        let max = self
            .leases
            .iter()
            .find(|lease| lease.lease_id == lease_id)
            .map_or(0, |lease| lease.max_inflight);
        // Read, never `entry(..).or_insert(0)`: a refusal must leave this map
        // EXACTLY as it found it. The line this replaced removed the key on a
        // cap refusal, which forgot the count of the `max` requests that were
        // in flight at that very moment, so the next `enter_relay` read zero,
        // admitted, and the cap stopped binding after the first request it
        // refused. And the `or_insert` itself parked a zero for a lease whose
        // `max_inflight` is zero, against the rule `Self::leave_relay`
        // documents: zero and absent are the same fact here.
        let in_flight = self.inflight(lease_id);
        if in_flight >= max {
            // The served pair stays recorded. A refused request was not served,
            // so its id could honestly be retried, but a cap refusal is the
            // one refusal a borrower is told to retry AFTER
            // (`LeaseRefusal::InFlightFull`), and it retries with a fresh id
            // because `open_serve` mints one per call. Keeping it is the safe
            // direction: forgetting it would hand a replayer a free slot per
            // refusal.
            return Err(RelayRefusal::TooManyInflight { max });
        }
        self.inflight.insert(lease_id, in_flight + 1);
        Ok(())
    }

    /// Record a `(lease_id, request_id)` pair and report whether it is NEW.
    ///
    /// `false` means this exact request has already been admitted for relay
    /// here. Bounded by [`SERVED_CAPACITY`] and [`SERVED_TTL_MS`]. See
    /// [`Self::pairs`], which carries every defect this closes.
    fn admit_served(&mut self, lease_id: u128, request_id: u128, now_ms: i64) -> bool {
        Self::admit_pair(&mut self.pairs, lease_id, request_id, now_ms)
    }

    /// One admission into one of the two per-lease caches: expire, refuse a
    /// repeat, evict this LEASE's oldest id if it is at its own bound, and
    /// record.
    ///
    /// **Every eviction is inside one lease.** The cache was a single FIFO, so
    /// a borrower with requests of its own to make pushed other borrowers'
    /// pairs out of it and turned their replay defence off; and once it had
    /// filled the queue by itself it evicted its own older ids, which it could
    /// then send again for free. A per-lease bound makes the only id a
    /// borrower can lose one of its own, at which point the pair is older than
    /// [`SERVED_PER_LEASE_CAPACITY`] of its own requests and a replay of it
    /// buys a request it has already paid for.
    ///
    /// The process-wide bound is kept as well: a ledger holding more than
    /// [`SERVED_CAPACITY`] ids across all leases drops the lease whose oldest
    /// id is oldest. The number of leases is capped
    /// ([`MAX_LEASES_PER_PEER`] per peer) and every entry expires at
    /// [`SERVED_TTL_MS`], so this is a ceiling rather than the working
    /// mechanism.
    fn admit_pair(
        cache: &mut HashMap<u128, VecDeque<RelayedPair>>,
        lease_id: u128,
        request_id: u128,
        now_ms: i64,
    ) -> bool {
        cache.retain(|_, ids| {
            ids.retain(|pair| now_ms.saturating_sub(pair.at_ms) < SERVED_TTL_MS);
            !ids.is_empty()
        });
        while cache.values().map(VecDeque::len).sum::<usize>() >= SERVED_CAPACITY {
            let Some(oldest) = cache
                .iter()
                .filter_map(|(lease, ids)| ids.front().map(|pair| (*lease, pair.at_ms)))
                .min_by_key(|(_, at)| *at)
                .map(|(lease, _)| lease)
            else {
                break;
            };
            if let Some(ids) = cache.get_mut(&oldest) {
                ids.pop_front();
                if ids.is_empty() {
                    cache.remove(&oldest);
                }
            }
        }
        let ids = cache.entry(lease_id).or_default();
        if ids.iter().any(|pair| pair.request_id == request_id) {
            return false;
        }
        while ids.len() >= SERVED_PER_LEASE_CAPACITY {
            ids.pop_front();
        }
        ids.push_back(RelayedPair {
            request_id,
            at_ms: now_ms,
            charged: false,
        });
        true
    }

    /// How many `(lease_id, request_id)` pairs are remembered right now, so a
    /// gate can read the bound off the ledger the relay decided on rather than
    /// off a copy.
    pub fn served_len(&self) -> usize {
        self.pairs.values().map(VecDeque::len).sum()
    }

    /// How many request ids are remembered for ONE lease, so a gate can read
    /// the per-lease bound off the ledger rather than off a copy of it.
    pub fn served_len_for(&self, lease_id: u128) -> usize {
        self.pairs.get(&lease_id).map_or(0, VecDeque::len)
    }

    /// Mark one remembered pair CHARGED, and report whether this call is the
    /// one that charged it.
    ///
    /// `false` means the charge for this pair has already landed, which is what
    /// makes a retried or diamond-delivered relay debit once.
    ///
    /// A pair this ledger does not remember is CHARGED and recorded as charged,
    /// which is the only way the idempotence survives an
    /// eviction that two separate maps used to break. An absent pair means one of two things and both
    /// want the same answer: the relay was admitted and its entry has since
    /// aged out (so nobody has charged it, and answering `false` would be the
    /// free relay one map exists to prevent), or nothing admitted it at all (so
    /// this is the first charge). Recording it is what makes the SECOND call
    /// for the same pair answer `false`.
    fn mark_charged(&mut self, lease_id: u128, request_id: u128) -> bool {
        if let Some(pair) = self
            .pairs
            .get_mut(&lease_id)
            .and_then(|ids| ids.iter_mut().find(|pair| pair.request_id == request_id))
        {
            if pair.charged {
                return false;
            }
            pair.charged = true;
            return true;
        }
        // The same bounded insert [`Self::admit_served`] uses, so a pair that
        // arrives here alone cannot grow this map past its own ceiling. The
        // clock is this process's wall clock, the one every production caller
        // of [`Self::enter_relay`] stamps its entries with.
        Self::admit_pair(&mut self.pairs, lease_id, request_id, crate::now_ms());
        if let Some(pair) = self
            .pairs
            .get_mut(&lease_id)
            .and_then(|ids| ids.iter_mut().find(|pair| pair.request_id == request_id))
        {
            pair.charged = true;
        }
        true
    }

    /// How many remembered pairs have been charged. See [`Self::served_len`].
    pub fn debited_len(&self) -> usize {
        self.pairs
            .values()
            .flat_map(VecDeque::iter)
            .filter(|pair| pair.charged)
            .count()
    }

    /// Release one slot taken by [`Self::enter_relay`].
    ///
    /// Saturating rather than wrapping: an unpaired release is a caller bug, and
    /// a counter that wraps to 255 would strand the lease for its whole TTL.
    pub fn leave_relay(&mut self, lease_id: u128) {
        if let Some(in_flight) = self.inflight.get_mut(&lease_id) {
            *in_flight = in_flight.saturating_sub(1);
            // The review's M4: the key was kept at zero, so this map grew one
            // entry per lease and never shrank. Zero and absent are the same
            // fact (`Self::inflight` answers `0` for a missing key), so the
            // entry is dropped rather than parked.
            if *in_flight == 0 {
                self.inflight.remove(&lease_id);
            }
        }
    }

    /// How many relayed requests are in flight against one lease, for the panel
    /// row and `tcr peer ls --json`.
    pub fn inflight(&self, lease_id: u128) -> u8 {
        self.inflight.get(&lease_id).copied().unwrap_or(0)
    }

    /// How many leases have an in-flight entry at all, so a gate can read the
    /// rule [`Self::leave_relay`] documents, zero and absent are the same
    /// fact, off the ledger rather than off a copy of it. Same shape and same
    /// reason as [`Self::served_len`].
    pub fn inflight_tracked(&self) -> usize {
        self.inflight.len()
    }

    /// May this relayed request proceed against this lease?
    ///
    /// The order is the contract: [`LeaseRefusal::LeaseExpired`], then
    /// [`LeaseRefusal::LeaseSpent`], then [`LeaseRefusal::OwnerGuard`]. The
    /// third is not a consequence of the first two and must be able to fire
    /// alone.
    /// A lease id this ledger does not hold answers
    /// [`LeaseRefusal::LeaseExpired`] rather than a variant of its own. The
    /// lender's ledger is the authority and it drops rows it no longer honours,
    /// so "I have never heard of this lease" and "that lease is over" are the
    /// same fact told to the borrower, and a separate variant would invite a
    /// borrower to retry one of them.
    ///
    /// A unit this build refuses ([`LeaseUnit::Tokens`],
    /// [`LeaseUnit::Unknown`]) answers [`LeaseRefusal::Unsupported`], checked
    /// AFTER expiry and BEFORE spend: a dead lease is dead whatever its unit,
    /// but there is no honest way to compare a spend figure against a budget
    /// this build cannot read.
    pub fn may_relay(&self, lease_id: u128, now_ms: i64) -> Result<(), LeaseRefusal> {
        let Some(lease) = self.leases.iter().find(|lease| lease.lease_id == lease_id) else {
            return Err(LeaseRefusal::LeaseExpired);
        };
        if lease.expires_at_ms <= now_ms {
            return Err(LeaseRefusal::LeaseExpired);
        }
        // The end, checked beside the renewal deadline and BEFORE the
        // spend: at `until` the lender stops renewing, so a lease whose end has
        // passed is over whatever is left on its budget.
        //
        // The answer is `LeaseExpired` and not a variant of its own. A
        // `LeaseEnded` would be the honest word and it would be a NEW variant of
        // `LeaseRefusal`, which is in `tcr_peer_wire`, and nothing here adds a
        // variant there. `LeaseExpired`'s own doc is "the lease's absolute deadline has
        // passed", which `until` is, and the borrower's action is identical:
        // stop spending this lease.
        if lease.until.is_some_and(|until| until <= seconds_of(now_ms)) {
            return Err(LeaseRefusal::LeaseExpired);
        }
        let Some(budget) = fraction_budget(lease.unit) else {
            return Err(LeaseRefusal::Unsupported);
        };
        if lease.spent >= budget {
            return Err(LeaseRefusal::LeaseSpent);
        }
        // THE THIRD CHECK, AND IT FIRES ALONE. Reached with budget left on the
        // lease and the deadline still ahead, because the owner's guard band is
        // the owner's and a lease can never spend it. An absent measurement is a
        // refusal: see `Ledger::headroom`.
        //
        // Read for THIS LEASE'S SCOPE, not for the fleet: a lease
        // scoped to a group whose accounts are spent is refused even while the
        // pool has room, which is the whole of what scoping one means.
        match self.headroom_for(&self.scope_of(lease_id), lease.window) {
            Some(headroom) if headroom > 0.0 => Ok(()),
            _ => Err(LeaseRefusal::OwnerGuard),
        }
    }

    /// Note the path this lease's borrows are being served over.
    ///
    /// Called by whatever holds the stream, which is the only layer that knows
    /// which of the borrower's endpoints answered, and called again whenever
    /// that changes. Unknown is [`Option::None`] at the CALLER: there is no
    /// clearing call, because "this borrow arrived and I could not attribute
    /// it" must not erase the attribution of the borrow before it.
    pub fn note_lease_path(&mut self, lease_id: u128, path: crate::peer::config::Locator) {
        self.lease_paths.insert(lease_id, path);
    }

    /// Tokens this lease's charge is worth, or `None` when the lease is not
    /// measured in tokens.
    ///
    /// A [`LeaseUnit::Fraction`] lease has NO token count: it is a share of the
    /// owner's window, and the number of tokens that share buys is a fact about
    /// upstream's pricing that nothing on this Mac holds. Converting one would
    /// be inventing the figure, which is the distinction
    /// [`crate::status::PathStatus::tokens_per_hour`] is built on.
    fn tokens_of(unit: LeaseUnit, charge: f64) -> Option<u64> {
        let LeaseUnit::Tokens(amount) = unit else {
            return None;
        };
        let tokens = (amount as f64) * charge.clamp(0.0, 1.0);
        if !tokens.is_finite() || tokens <= 0.0 {
            return None;
        }
        // Bounded by the clamp above and by `amount`, so the cast cannot
        // saturate into a figure nobody granted.
        Some(tokens.round() as u64)
    }

    /// Debit one relayed request, exactly once.
    ///
    /// `observed_rise` is the utilization delta the lender measured on its own
    /// account; `request_id` is what makes a retry or a diamond delivery debit
    /// once.
    /// Returns what THIS call charged, so `0.0` means "already debited, nothing
    /// added" rather than "free". The running total is [`Lease::spent`], and a
    /// caller that wants it reads the lease.
    ///
    /// A negative `observed_rise`, a window that reset under the request, which
    /// the utilization headers really do report, charges [`MIN_DEBIT`] like any
    /// other sub-step rise. It never credits the lease: a reset is the owner's
    /// windfall, not the borrower's.
    pub fn debit(&mut self, lease_id: u128, _request_id: u128, observed_rise: f64) -> f64 {
        // The charge stays idempotent per pair, which is the invariant this
        // module's own docs state ("a retried or diamond-delivered relay debits
        // once, keyed on the request id"). What CHANGED for the review's H2 is
        // that this is no longer the only place the pair is looked at:
        // `enter_relay` refuses a replayed pair before the request is served, so
        // "debited once" and "served once" are now two facts about one
        // remembered pair instead of one sentence covering for the other. Both are kept, a replay cannot reach
        // here any more, and an accounting key that only one layer enforces is
        // an accounting key that stops being enforced the day that layer moves.
        //
        // Bounded, unlike the `HashSet` this was (the review's M4): a peer grew
        // it one entry per relayed request for the life of the process.
        //
        // And it reads the SAME map `enter_relay` wrote. The charge used to
        // keep a map of its own, filled only by the
        // relays that got this far and swept on the wall clock rather than on
        // the caller's, so the two evicted different pairs at different
        // moments and a pair the serve half had forgotten was served again for
        // nothing. There is no clock here any more because there is nothing
        // here to expire.
        if !self.mark_charged(lease_id, _request_id) {
            return 0.0;
        }
        let charge = if observed_rise > MIN_DEBIT {
            observed_rise
        } else {
            MIN_DEBIT
        };
        // An INDEX rather than a `&mut` row, because `self.persist()` below
        // needs `&self` and a live mutable borrow of `self.leases` would hold
        // the whole struct.
        let Some(at) = self
            .leases
            .iter()
            .position(|lease| lease.lease_id == lease_id)
        else {
            // No row to charge. The pair `enter_relay` recorded still stands,
            // so a retry of the same request cannot find a restored row and
            // charge it twice.
            return 0.0;
        };
        self.leases[at].spent += charge;
        // The per-path half. Three things have to be true for a charge to be
        // attributable: the lease is measured in tokens, the serving side noted
        // a path, and the ledger knows whose lease it is. Every one of the three
        // is absent on some real borrow, so the arithmetic is skipped rather
        // than defaulted. A default here would put somebody else's tokens on a
        // path they never went over.
        if let (Some(tokens), Some(path), Some(peer)) = (
            Self::tokens_of(self.leases[at].unit, charge),
            self.lease_paths.get(&lease_id).copied(),
            self.grantees.get(&lease_id).copied(),
        ) {
            if let Ok(mut meter) = crate::peer::tunnel::path_meter().lock() {
                meter.charge_tokens(&peer, path, tokens, crate::now_ms());
            } else {
                // Never silent, and never a refused relay: the request is
                // already served and the lease is already charged by the time
                // this runs. See the same trade in `handle_forward_on`.
                tracing::error!(
                    "peer lease: the per-path meter's lock is poisoned, so this lease's tokens \
                     are missing from every per-path figure until restart"
                );
            }
        }
        // A moved `spent` is the ceiling H2 exists to keep binding, so it is the
        // one figure a restart must not forget, but it is NOT written here.
        // This runs with the ledger's mutex held, once per relayed request, and
        // `persist` is a locked file round trip. See `Self::dirty` and
        // `Self::flush`.
        self.dirty = true;
        charge
    }

    /// Apply a borrower's [`Control::UsageHint`](tcr_peer_wire::Control::UsageHint) to one lease.
    ///
    /// # Why the owner needs it at all
    ///
    /// In `serve` mode the owner sees the response and debits from the
    /// utilization rise it measured itself ([`Self::debit`]). A `hand`-mode
    /// response never reaches the owner, so nothing it can observe moves, and a
    /// lease whose `spent` never rises is a lease that never ends. The borrower
    /// reports the rise it saw and this adds it.
    ///
    /// # It can only ever RAISE `spent`
    ///
    /// The reporter is the party that gains by under-reporting, so a negative
    /// or zero hint is dropped rather than credited: the worst a lying borrower
    /// can do here is report nothing, which is the same position the owner is
    /// in without the frame at all. `until`, the lease expiry and the owner's
    /// choice to stop renewing all bind without any hint ever arriving.
    ///
    /// **Reconciliation against the owner's real usage probe
    /// (`src/manager/probing.rs`) is OUT OF SCOPE here.** That is the
    /// check that would turn this hint into an accounting figure, and until it
    /// exists a hand-mode lease is trusted to the limit of its `until`.
    ///
    /// Returns what it charged, so `0.0` means "nothing applied": an unknown
    /// lease id, a lease this ledger does not hold, or a hint that was not a
    /// positive, finite number.
    pub fn apply_usage_hint(&mut self, lease_id: u128, hinted: f64) -> f64 {
        if !hinted.is_finite() || hinted <= 0.0 {
            return 0.0;
        }
        let Some(at) = self
            .leases
            .iter()
            .position(|lease| lease.lease_id == lease_id)
        else {
            return 0.0;
        };
        self.leases[at].spent += hinted;
        // Same reason `debit` sets this rather than writing: this runs with the
        // ledger's mutex held and `persist` is a locked file round trip.
        self.dirty = true;
        hinted
    }

    /// Which window one lease is against, or `None` for a lease this ledger
    /// does not hold. For the lender's log line, which names the window and
    /// never the path or the model.
    pub fn window_of(&self, lease_id: u128) -> Option<Window> {
        self.leases
            .iter()
            .find(|lease| lease.lease_id == lease_id)
            .map(|lease| lease.window)
    }

    /// Every live lease, for `tcr peer ls --json` and the panel's activity
    /// line.
    ///
    /// A lease is live on a wall clock and nothing else: there is no heartbeat,
    /// no failure detector and no peer-down event anywhere in this design,
    /// because a sleeping laptop delivers no death notice and inferring one is
    /// how a lease turns into a timeout with extra steps.
    pub fn live(&self, now_ms: i64) -> Vec<Lease> {
        self.leases
            .iter()
            .copied()
            .filter(|lease| lease.expires_at_ms > now_ms)
            .collect()
    }

    /// How much of one scope's window this Mac has already promised to peers
    /// and not yet seen spent.
    ///
    /// **The figure a new grant has to be cut out of what is LEFT, not out of
    /// the whole.** `Ledger::grant` read `headroom_for` alone, which is what
    /// this Mac's own fleet has left, and nothing about what it had already
    /// lent: a borrower that re-asked held up to [`MAX_LEASES_PER_PEER`]
    /// leases, each at the full fraction, and the lender promised several
    /// times the room it had.
    ///
    /// Counted as the UNSPENT part of every live lease on the same scope and
    /// window, `unit` minus `spent`, never below zero: a lease that has
    /// already been spent has had its cost taken out of the measured headroom
    /// by the quota itself, so counting it twice would make a lender stop
    /// lending after one busy borrower.
    ///
    /// A lease with a byte or token unit contributes nothing: it is not a
    /// fraction of a window and there is no honest conversion.
    ///
    /// This is the figure the comments in this file used to call
    /// `lent_fraction`, which never existed under that name.
    pub fn committed_fraction(&self, scope: &LendScope, window: Window, now_ms: i64) -> f64 {
        self.leases
            .iter()
            .filter(|lease| lease.expires_at_ms > now_ms)
            .filter(|lease| lease.window == window)
            .filter(|lease| {
                // EVERY LEASE OVER ACCOUNTS THIS SCOPE ALSO DRAWS FROM, not
                // only the ones whose scope string matches. This compared
                // `scope_key`s, so an `all` lease and a `group:work` lease over
                // the same accounts never subtracted from each other and the
                // same room was promised twice, while `headroom_for` combines
                // the two figures with `scoped.min(all)` and therefore treats
                // them as one pool. See [`scopes_overlap`].
                let held = self
                    .scopes
                    .get(&lease.lease_id)
                    .map_or(&LendScope::All, |scope| scope);
                scopes_overlap(scope, held)
            })
            .map(|lease| match lease.unit {
                LeaseUnit::Fraction(fraction) => (fraction - lease.spent).max(0.0),
                LeaseUnit::Tokens(_) | LeaseUnit::Unknown => 0.0,
            })
            .sum()
    }

    /// How many live leases `peer` holds here right now, what
    /// [`MAX_LEASES_PER_PEER`] is measured against.
    ///
    /// Counted off [`Self::live`] and the grantee map rather than off a
    /// per-peer counter, for the reason every other figure in this type is:
    /// a counter is a second answer that has to be decremented on expiry, on
    /// revoke and on restore, and the one that gets forgotten locks a borrower
    /// out of a lender that owes it nothing.
    pub fn live_for(&self, peer: &PeerId, now_ms: i64) -> usize {
        self.leases
            .iter()
            .filter(|lease| lease.expires_at_ms > now_ms)
            .filter(|lease| self.grantees.get(&lease.lease_id) == Some(peer))
            .count()
    }
}
