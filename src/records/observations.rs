//! What this boot has observed per provider: its upstream errors, by kind, over a fixed
//! ten-minute window, and the model ids it served.
//!
//! The manager holds one [`Observations`] and the proxy records into it as answers arrive
//! ([`ErrorKind::of_status`], [`ErrorKind::of_stream`]). The routing document reports it as
//! each provider's `lastError` and its catalogue of models. Nothing in selection reads it.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::base::provider::Provider;

/// The fixed `stream_error` kind `proxy::parse_sse_usage` synthesizes when a stream
/// ends without Anthropic's `message_stop` terminator and no more specific
/// in-band `error` event already explained why. Named so the proxy handler's
/// classifier can tell "we witnessed an explicit error" (always trustworthy)
/// apart from "we never saw the terminator" (trustworthy only when we also
/// know we saw everything there was to see — see the handler's use of this).
/// `pub(crate)` so the proxy writes, and [`ErrorKind::of_stream`] reads, this one spelling, not a copy.
pub(crate) const TRUNCATED_STREAM_ERROR_KIND: &str = "truncated";

/// `lastError.count10m`'s window, in seconds: ten minutes.
const ERROR_WINDOW_SECS: i64 = 600;

/// What an upstream error was, by where tcr observed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ErrorKind {
    /// The upstream said it is overloaded: a 529 (each step of `proxy`'s 529 ladder), a 503,
    /// or a stream that failed in-band as overloaded.
    Overloaded,
    /// No available account can serve the requested model now: a `codex` account refused the
    /// model for its plan. Not measured overload, and not proof the model is gone: an account
    /// that supports it may simply be unavailable right now.
    Capacity,
    /// A 429: a quota rejection or a rate limit.
    Quota,
    /// The credential was refused: a 401, or a 403 that holds the account out
    /// (`Manager::mark_refused`).
    Auth,
    /// No usable answer: the send failed, the body could not be read, or a stream proved
    /// truncated.
    Transport,
}

impl ErrorKind {
    const ALL: [Self; 5] = [
        Self::Overloaded,
        Self::Capacity,
        Self::Quota,
        Self::Auth,
        Self::Transport,
    ];

    /// The kind an upstream answer's status alone names, `None` for a status that names none.
    /// A 403 is not here: only the one that refuses the credential is auth, and
    /// `Manager::mark_refused` records that one.
    pub const fn of_status(status: u16) -> Option<Self> {
        match status {
            401 => Some(Self::Auth),
            429 => Some(Self::Quota),
            503 | 529 => Some(Self::Overloaded),
            _ => None,
        }
    }

    /// The kind a failed stream's recorded kind (`Manager::record_stream_error`) names: a
    /// proved truncation is transport, an in-band overloaded error is overloaded, and any other
    /// in-band error names none.
    pub fn of_stream(kind: &str) -> Option<Self> {
        match kind {
            TRUNCATED_STREAM_ERROR_KIND => Some(Self::Transport),
            "overloaded_error" | "server_is_overloaded" => Some(Self::Overloaded),
            _ => None,
        }
    }

    const fn index(self) -> usize {
        self as usize
    }
}

/// One provider's upstream errors: the last, and how many of each kind fell in each second
/// of the last ten minutes. Fixed memory, one slot per second of the window reused as the
/// window moves, so no burst can grow it.
#[derive(Debug, Clone)]
struct ErrorLog {
    last: Option<(ErrorKind, i64)>,
    /// `(second, counts by kind)` at slot `second % ERROR_WINDOW_SECS`. A slot holding an
    /// older second is stale, and the next write to it starts it over.
    slots: Vec<(i64, [u32; ErrorKind::ALL.len()])>,
}

impl Default for ErrorLog {
    fn default() -> Self {
        Self {
            last: None,
            slots: vec![(i64::MIN, [0; ErrorKind::ALL.len()]); ERROR_WINDOW_SECS as usize],
        }
    }
}

impl ErrorLog {
    fn record(&mut self, kind: ErrorKind, at_ms: i64) {
        let second = at_ms.div_euclid(1000);
        let slot = &mut self.slots[second.rem_euclid(ERROR_WINDOW_SECS) as usize];
        if slot.0 < second {
            *slot = (second, [0; ErrorKind::ALL.len()]);
        }
        // A slot already holding a LATER second means this one is a whole window old.
        if slot.0 == second {
            slot.1[kind.index()] = slot.1[kind.index()].saturating_add(1);
        }
        if self.last.is_none_or(|(_, last_ms)| at_ms >= last_ms) {
            self.last = Some((kind, at_ms));
        }
    }

    /// Errors of `kind` in the ten minutes ending at `now_ms`: the seconds in
    /// `(now - 600s, now]`.
    fn count(&self, kind: ErrorKind, now_ms: i64) -> u32 {
        let now_second = now_ms.div_euclid(1000);
        self.slots
            .iter()
            .filter(|(second, _)| *second > now_second - ERROR_WINDOW_SECS && *second <= now_second)
            .map(|(_, counts)| counts[kind.index()])
            .fold(0, u32::saturating_add)
    }

    /// The last error as `(kind, at_ms, count of that kind in the ten minutes ending at
    /// now_ms)`, or `None` when none was seen.
    fn last_with_count(&self, now_ms: i64) -> Option<(ErrorKind, i64, u32)> {
        self.last
            .map(|(kind, at_ms)| (kind, at_ms, self.count(kind, now_ms)))
    }
}

/// What this boot has observed that the document reports per provider: upstream errors and
/// the model ids served. The `Manager` holds it; nothing in selection reads it.
#[derive(Debug, Default)]
pub struct Observations {
    errors: [ErrorLog; 2],
    models: [BTreeSet<String>; 2],
}

impl Observations {
    /// Note an upstream error of `kind` on `provider` at `at_ms`.
    pub fn record_error(&mut self, provider: Provider, kind: ErrorKind, at_ms: i64) {
        self.errors[slot(provider)].record(kind, at_ms);
    }

    /// Note that `provider` served model `id`.
    pub fn record_model(&mut self, provider: Provider, id: &str) {
        self.models[slot(provider)].insert(id.to_owned());
    }

    /// The last error on `provider` as `(kind, at_ms, count of that kind in the ten minutes
    /// ending at now_ms)`, or `None` when none was seen this boot.
    pub fn last_error(&self, provider: Provider, now_ms: i64) -> Option<(ErrorKind, i64, u32)> {
        self.errors[slot(provider)].last_with_count(now_ms)
    }

    /// Every model id `provider` served this boot.
    pub fn model_ids(&self, provider: Provider) -> BTreeSet<String> {
        self.models[slot(provider)].clone()
    }
}

/// Each provider's place in [`Observations`].
const fn slot(provider: Provider) -> usize {
    match provider {
        Provider::Anthropic => 0,
        Provider::Codex => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed instant on a whole second, in ms.
    const AT: i64 = 1_800_000_000_000;

    /// **`count10m` counts the last error's kind in the ten minutes up to now, per
    /// provider, and only those.**
    #[test]
    fn count10m_counts_the_last_kind_in_the_ten_minutes_up_to_now() {
        let mut seen = Observations::default();
        // Exactly ten minutes before `AT` is outside `(now - 600s, now]`.
        seen.record_error(Provider::Anthropic, ErrorKind::Quota, AT - 600_000);
        seen.record_error(Provider::Anthropic, ErrorKind::Quota, AT - 599_000);
        seen.record_error(Provider::Anthropic, ErrorKind::Transport, AT - 1_000);
        seen.record_error(Provider::Anthropic, ErrorKind::Quota, AT - 500);
        seen.record_error(Provider::Codex, ErrorKind::Quota, AT - 400);
        assert_eq!(
            seen.last_error(Provider::Anthropic, AT),
            Some((ErrorKind::Quota, AT - 500, 2)),
            "the last error is quota, and two quota errors fell inside the window"
        );
        assert_eq!(
            seen.last_error(Provider::Codex, AT),
            Some((ErrorKind::Quota, AT - 400, 1)),
            "each provider keeps its own errors"
        );
        assert_eq!(
            seen.last_error(Provider::Anthropic, AT + 600_000),
            Some((ErrorKind::Quota, AT - 500, 0)),
            "ten minutes on, the last error is still named and counts nothing"
        );
        assert_eq!(
            Observations::default().last_error(Provider::Codex, AT),
            None
        );
    }

    /// **The window is fixed memory**: a burst over far longer than ten minutes keeps one
    /// slot per second, and an error a whole window older than its slot is not counted.
    #[test]
    fn the_error_window_is_bounded_and_forgets_what_it_overwrites() {
        let mut log = ErrorLog::default();
        for second in 0..3_000 {
            log.record(ErrorKind::Overloaded, AT + second * 1_000);
        }
        assert_eq!(log.slots.len(), 600);
        assert_eq!(log.count(ErrorKind::Overloaded, AT + 2_999_000), 600);
        log.record(ErrorKind::Overloaded, AT);
        assert_eq!(
            log.count(ErrorKind::Overloaded, AT + 2_999_000),
            600,
            "a late error a window old lands on a slot a later second holds"
        );
        assert_eq!(log.last, Some((ErrorKind::Overloaded, AT + 2_999_000)));
    }

    /// **A status alone names auth, quota and overloaded, and nothing else.** A 403 is
    /// recorded only where it refuses the credential.
    #[test]
    fn a_status_names_only_the_kinds_it_can() {
        assert_eq!(ErrorKind::of_status(401), Some(ErrorKind::Auth));
        assert_eq!(ErrorKind::of_status(429), Some(ErrorKind::Quota));
        assert_eq!(ErrorKind::of_status(529), Some(ErrorKind::Overloaded));
        assert_eq!(ErrorKind::of_status(503), Some(ErrorKind::Overloaded));
        for status in [200, 400, 403, 404, 500, 502] {
            assert_eq!(ErrorKind::of_status(status), None, "{status}");
        }
    }

    /// **A failed stream names transport when it proved truncated and overloaded when it
    /// failed in-band as overloaded; any other in-band error names none.**
    #[test]
    fn a_stream_names_transport_or_overloaded_and_nothing_else() {
        assert_eq!(
            ErrorKind::of_stream("truncated"),
            Some(ErrorKind::Transport)
        );
        assert_eq!(
            ErrorKind::of_stream("overloaded_error"),
            Some(ErrorKind::Overloaded)
        );
        assert_eq!(
            ErrorKind::of_stream("server_is_overloaded"),
            Some(ErrorKind::Overloaded)
        );
        assert_eq!(ErrorKind::of_stream("context_length_exceeded"), None);
        assert_eq!(ErrorKind::of_stream("unknown"), None);
    }

    /// **Every served model id is remembered per provider, including later families.**
    #[test]
    fn model_ids_are_kept_per_provider_without_truncation() {
        let mut seen = Observations::default();
        seen.record_model(Provider::Codex, "gpt-a");
        seen.record_model(Provider::Codex, "gpt-a");
        assert_eq!(seen.model_ids(Provider::Codex).len(), 1);
        assert!(seen.model_ids(Provider::Anthropic).is_empty());
        for n in 0..100 {
            seen.record_model(Provider::Anthropic, &format!("claude-opus-{n}"));
        }
        assert_eq!(seen.model_ids(Provider::Anthropic).len(), 100);
        seen.record_model(Provider::Anthropic, "claude-sonnet-5-5");
        assert!(
            seen.model_ids(Provider::Anthropic)
                .contains("claude-sonnet-5-5")
        );
    }
}
