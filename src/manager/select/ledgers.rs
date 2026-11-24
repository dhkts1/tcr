//! The two memory-only maps selection writes: per-connection affinity and the divert episode ledger.

use super::*;

/// Idle TTL for a [`Manager::conn_affinity`] entry: an entry older than this is
/// treated as absent (the connection is presumed gone). Short on purpose — this
/// map exists only to keep NOISE traffic (`/api/event_logging`, `/mcp-registry`)
/// on one account for the lifetime of one connection, not to survive a
/// reconnect the way [`Manager::affinity`] does.
const CONN_AFFINITY_TTL_MS: i64 = 5 * 60 * 1000;

impl Manager {
    /// Lookup half of [`Manager::conn_affinity`]: the account this connection
    /// was last served on, or `None` if never recorded or the entry has aged
    /// past [`CONN_AFFINITY_TTL_MS`] (treated as absent — the connection is
    /// presumed gone). Does NOT check the account's current eligibility; the
    /// caller ([`Manager::select`]'s noise-routing overlay) does that itself so
    /// it can log/fall through distinctly.
    pub(in crate::manager) fn conn_affinity_get(
        &self,
        conn_key: u64,
        now_ms: i64,
    ) -> Option<usize> {
        let map = self
            .conn_affinity
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        map.get(&conn_key).and_then(|&(idx, touch)| {
            if now_ms.saturating_sub(touch) < CONN_AFFINITY_TTL_MS {
                Some(idx)
            } else {
                None
            }
        })
    }

    /// Write half of [`Manager::conn_affinity`]: record (or refresh) which
    /// account served connection `conn_key`, then evict the single
    /// oldest-last-touch entry if the map grew past [`CONN_AFFINITY_CAP`]
    /// ([`CappedMap::insert`]), on this SEPARATE, never-persisted map. Never touches `self.affinity` and
    /// never marks it dirty.
    pub(in crate::manager) fn conn_affinity_record(&self, conn_key: u64, idx: usize, now_ms: i64) {
        let mut map = self
            .conn_affinity
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        map.insert(conn_key, (idx, now_ms));
    }

    /// Write half of [`Manager::divert_ledger`] (§4.1/§4.2): record `dest` as
    /// a destination this session was diverted to, in the episode identified
    /// by `(pin, until_ms)`. Called only AFTER the affinity guard for this
    /// request has already dropped — never nested under it or the accounts
    /// lock (§4.2 rule 3; this method takes only the ledger's own lock).
    ///
    /// Reset is structural, not timed (§4.1): if the stored entry's
    /// `(pin, until_ms)` no longer matches, it is a stale episode and is
    /// overwritten from scratch rather than merged into. On a genuinely new
    /// episode (or after that reset) the first `dest` recorded becomes
    /// [`DivertEpisode::sticky`]; every later `dest` in the same episode only
    /// grows the `destinations` mask — `sticky` stays the FIRST destination,
    /// by design (§4.1: "so the sticky overlay has a single preferred index
    /// without scanning the mask").
    ///
    /// Bounded by [`DIVERT_LEDGER_CAP`] + LRU-by-`until_ms` eviction
    /// ([`CappedMap::evict_sparing`]) on this separate, never-persisted map.
    pub(in crate::manager) fn record_divert(
        &self,
        session_key: u64,
        pin: usize,
        until_ms: i64,
        dest: usize,
        _now_ms: i64,
    ) {
        // The mask is a u64; `DivertEpisode::destinations`' own doc-comment
        // caps this at 64 accounts, "nowhere near" the fleet size. Fail safe
        // rather than panic on an out-of-range shift: the episode simply
        // isn't recorded for this one destination, so sticky never offers it
        // (it would have failed the ordinary eligibility gate anyway once the
        // fleet ever did reach 64 accounts).
        let Some(bit) = 1u64.checked_shl(dest as u32) else {
            return;
        };
        let mut ledger = self
            .divert_ledger
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let entry = ledger
            .entry(session_key)
            .and_modify(|e| {
                if e.pin != pin || e.until_ms != until_ms {
                    *e = DivertEpisode {
                        pin,
                        until_ms,
                        destinations: 0,
                        sticky: dest,
                    };
                }
            })
            .or_insert(DivertEpisode {
                pin,
                until_ms,
                destinations: 0,
                sticky: dest,
            });
        if entry.destinations == 0 {
            entry.sticky = dest;
        }
        entry.destinations |= bit;
        ledger.evict_sparing(session_key);
    }
}
