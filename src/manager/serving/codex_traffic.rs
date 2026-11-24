//! The ten-minute counts of Codex requests this process refused itself or relayed unpooled.

use crate::manager::*;

/// How many one-minute buckets a [`MinuteRing`] holds: the minutes it counts back.
const TRAFFIC_RING_MINUTES: i64 = 10;

/// A count over the last ten minutes, in ten one-minute buckets keyed by the minute
/// (`now_ms / 60_000`). A bucket counts only while its minute is one of the last ten, so a
/// quiet stretch empties the count with no sweep; a bump that lands on a slot holding an
/// older minute resets it first.
#[derive(Debug, Default)]
pub(in crate::manager) struct MinuteRing {
    buckets: [(i64, u64); TRAFFIC_RING_MINUTES as usize],
}

impl MinuteRing {
    const fn bump(&mut self, now_ms: i64) {
        let minute = now_ms.div_euclid(60_000);
        let slot = &mut self.buckets[minute.rem_euclid(TRAFFIC_RING_MINUTES) as usize];
        if slot.0 != minute {
            *slot = (minute, 0);
        }
        slot.1 += 1;
    }

    fn count(&self, now_ms: i64) -> u64 {
        let minute = now_ms.div_euclid(60_000);
        self.buckets
            .iter()
            .filter(|(at, _)| *at <= minute && minute - *at < TRAFFIC_RING_MINUTES)
            .map(|(_, n)| n)
            .sum()
    }
}

/// The Codex 429s this process sent itself: their ten-minute count, and the latest one's
/// error `type` and when it was sent.
#[derive(Debug, Default)]
pub(in crate::manager) struct CodexRefusals {
    ring: MinuteRing,
    last: Option<(tcr_status_wire::CodexRefusalKind, i64)>,
}

impl Manager {
    /// Count a 429 this process answered a Codex request with itself, under its body's kind.
    /// Fed by `proxy::exhausted_response`'s Codex arms.
    pub fn note_codex_refusal(&self, kind: tcr_status_wire::CodexRefusalKind, now_ms: i64) {
        let mut refused = self
            .codex_refused
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        refused.ring.bump(now_ms);
        refused.last = Some((kind, now_ms));
    }

    /// Count a Codex request relayed on the client's own credential, unpooled, because it named
    /// a routing of its own (`x-openai-account-routing-override` or `x-openai-fedramp`).
    pub fn note_codex_unpooled(&self, now_ms: i64) {
        self.codex_unpooled
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .bump(now_ms);
    }

    /// What this process did with Codex requests in the ten minutes before `now_ms`: the 429s
    /// it sent itself, and the requests it relayed unpooled. The latest refusal's type and time
    /// are reported only while the refused count is above zero, so the wire never pairs a zero
    /// count with a stale type.
    ///
    /// Both counts live in memory and a restart empties them: until the next refusal, a
    /// restarted proxy reports none, though codex may still be failing.
    pub fn codex_traffic(&self, now_ms: i64) -> tcr_status_wire::CodexTraffic {
        let (refused_10m, last) = {
            let refused = self
                .codex_refused
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let count = refused.ring.count(now_ms);
            (count, refused.last.filter(|_| count > 0))
        };
        let unpooled_10m = self
            .codex_unpooled
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .count(now_ms);
        let (last_refusal, last_refusal_at_ms) = match last {
            Some((error_type, at_ms)) => (Some(error_type), Some(at_ms)),
            None => (None, None),
        };
        tcr_status_wire::CodexTraffic {
            refused_10m,
            last_refusal,
            last_refusal_at_ms,
            unpooled_10m,
        }
    }
}
