//! The axum catch-all handler: authenticate, select an account, refresh its
//! token, forward the request to Anthropic, and stream the response back
//! **unchanged** while a side consumer parses SSE usage.
//!
//! Behaviours by design (see `DESIGN.md` + the sweep notes):
//! - The client's inbound `authorization` header is **stripped** before we set
//!   our own `Bearer` (never forward a client token upstream).
//! - An upstream `401` force-refreshes the serving account's token **once**. If a
//!   new token was applied it retries the SAME account; if the force was
//!   coalesced/cooldown-suppressed or the refresh failed (no new token) it rotates
//!   away WITHOUT sidelining — a healthy account is never marked `Error` for losing
//!   a race to the refresh throttle. A second `401` rotates too — it is rotation
//!   churn, not proof of death; only a REJECTED refresh sets `Error`. The one
//!   exception is an account with no refresh token (`tcr login --token`): its
//!   token can neither churn nor be renewed, so a single `401` marks it `Error`.
//!   A stale-token `401` is never passed to the client.
//! - A `429` with a `rejected` unified status is durable quota exhaustion →
//!   throttle + rotate; a transient `429` waits (bounded) and retries the same
//!   account, or rotates once the wait would be too long.
//! - Token usage is counted on the **true serving account** and the input count
//!   includes `cache_creation_input_tokens` + `cache_read_input_tokens` (the JS
//!   proxy counted only `input_tokens`). It is applied **once** per served
//!   request, so a retry that rotated accounts never double-counts.
//! - When every account attempt fails in transport (`bad_gateway`), the shape
//!   tells the client whether resending is free: `503` + `retry-after` +
//!   `x-should-retry: true` when every failure died at connect (nothing ever
//!   left the box); `502` + `x-should-retry: false` when at least one attempt
//!   failed AFTER connecting, so a connection existed and the request's fate
//!   upstream is unknown — a blind retry there risks double-sending a POST.
//!   `offline_unavailable` (DNS/name-resolution failure — a fact about this
//!   machine, not any account) is unaffected and stays a `503`.

use std::collections::HashSet;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::ops::ControlFlow;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HOST, USER_AGENT};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde_json::Value;
use tcr_status_wire::CodexRefusalKind;
use time::OffsetDateTime;

use crate::bound_tokens::BoundTokenKind;
use crate::config;
use crate::fallback;
use crate::http_client::parse_retry_after;
use crate::manager::{
    AccountStatus, AddAccountOutcome, AddPersist, ControlPersist, DisablePersist, ExhaustionHint,
    InFlightGuard, Manager, MintedToken, SetControlOutcome, SetDisabledOutcome, SetPriorityOutcome,
};
use crate::observations::TRUNCATED_STREAM_ERROR_KIND;
use crate::provider::Provider;
use crate::quota::{UnifiedRejectionKind, quota_rejections};
use crate::request_key::{ProxySecrets, key_matches};
use crate::stats::{GateReason, SessionKind, describe_gates, gate_phrase};

mod client_error;
mod ladder;
pub(crate) mod served;

use client_error::{ClientError, ErrorKind};
use ladder::{Ladder, Step, Wants};
use served::{ServedRequest, ServedTurn, WireFold, record_served_request};

/// Cap on a buffered request body (256 MiB) — a single-user localhost proxy has
/// no legitimate payload near this, but an unbounded read is a DoS surface. The
/// non-stream *response* body is bounded by the same cap (see [`read_capped_body`]).
const MAX_BODY_BYTES: usize = 256 * 1024 * 1024;
/// Cap on the upstream error body copied into a log line (see
/// [`log_upstream_error_body`]). Large enough to carry Anthropic's whole error
/// envelope (`{"type":"error","error":{"type":...,"message":...}}`, typically
/// well under 1 KiB) with room to spare, small enough that a log line never
/// carries an upstream's full multi-KiB HTML error page.
const ERROR_BODY_LOG_CAP: usize = 2 * 1024;
/// Most bytes of an upstream `429` body read just to log it. A 429 is always
/// discarded after logging (every path out of its arm rotates or retries), so
/// this only has to hold Anthropic's small JSON envelope; a larger body is not
/// logged at all rather than buffered at [`MAX_BODY_BYTES`].
const RATE_LIMIT_BODY_CAP: usize = 64 * 1024;
/// Seconds to wait for an upstream `429` body before giving up on logging it.
/// The arm returned without reading the body before it logged one, so a body
/// that never finishes must not stall the rotation it did not stall before.
const RATE_LIMIT_BODY_READ_SECS: u64 = 5;
/// Header `tcr run --group <name>` sets via `ANTHROPIC_CUSTOM_HEADERS` (see
/// `src/main.rs`'s `compose_group_header`), which Claude Code forwards
/// verbatim as a real outbound header on every `/v1/messages` request. `pub`
/// so `main.rs` — a separate (binary) crate — shares this one definition
/// rather than duplicating the header name as a second literal that could
/// drift out of sync with what this file reads.
pub const GROUP_HEADER_NAME: &str = "x-tcr-group";
/// The account-set restriction a BORROWED request carries, and the only
/// selection header that names accounts (a scope's
/// `--scope account:<label>[,<label>]`).
///
/// Written by the lender's own serving leg
/// ([`crate::peer::serve::serve_on_own_account`]) onto the loopback request it
/// makes through this proxy, after its header allowlist has run, a borrower
/// cannot set it, because `lender_forwards_header` does not carry this name.
///
/// The value is a comma-separated list of
/// [`tcr_peer_wire::sanitize_label`] labels, the same spelling the CLI's
/// `--scope` takes and `tcr peer ls --json` prints. It can only ever NARROW
/// the pick: every account outside the set is benched before the rotation loop
/// starts (see `handle`), so every gate that already holds, reserved, parked,
/// disabled, the model class, pacing, still holds on top of it, and a set
/// that names no live account ends in the same honest 429 an exhausted fleet
/// produces.
pub const ACCOUNTS_HEADER_NAME: &str = "x-tcr-accounts";
/// The marker a lender puts on a relayed request as it posts it into its OWN
/// proxy, so that proxy knows the request has already crossed one Mac.
///
/// # What it stops
///
/// A relayed request is served by re-posting it onto the lender's own loopback
/// proxy, which is what makes the lender's picker, its Bearer and its bucket
/// the ones that serve it. That proxy is a full proxy: if the lender's own
/// fleet is dry the request walks to the dry-fleet terminal, and that terminal
/// consults a fallback provider of its own. So a borrowed request could be
/// borrowed onward, to a third Mac or back to the Mac that sent it, on a mesh
/// where everyone is dry: each hop looks locally reasonable and the cycle is
/// only visible from outside.
///
/// One hop is the rule, and this marker is how the proxy knows which request it
/// is looking at. Presence is the whole signal; the value is written for a
/// human reading a capture and is never parsed.
///
/// **A client that forges it can only refuse itself a borrow**, which is the
/// safe direction: it turns its own fallback off and gets the honest exhausted
/// answer. That is why this is a marker rather than a counter a peer could
/// decrement.
pub const RELAYED_HEADER_NAME: &str = "x-tcr-relayed";
/// Depth of the SSE tee side-channel. A fast upstream feeding a slow parser task
/// can retain at most this many chunks; beyond it, `try_send` drops chunks for
/// the parser (usage counting becomes best-effort) rather than letting the buffer
/// grow without bound or gating the client passthrough on the parser.
const SSE_TEE_CAPACITY: usize = 256;
/// The content-block type Anthropic uses for a tool IT runs — `--advisor` today, `web_search`
/// next. Its result block travels with an `encrypted_content` that only the minting
/// organization can decrypt, which is why the id has to be remembered at all: see
/// [`crate::bound_tokens`].
const SERVER_TOOL_USE_BLOCK_TYPE: &str = "server_tool_use";
/// A reasoning block. Its `signature` is Anthropic's attestation over the block, arriving in
/// the stream as `signature_delta` fragments; another account answers a history carrying it
/// with `400 thinking or redacted_thinking blocks in the latest assistant message cannot be
/// modified`.
const THINKING_BLOCK_TYPE: &str = "thinking";
/// A reasoning block the API returns already encrypted; its `data` binds exactly as a
/// `signature` does.
const REDACTED_THINKING_BLOCK_TYPE: &str = "redacted_thinking";
/// A transient `429` whose `retry-after` is within this bound is waited out
/// inline on the same account; anything longer throttles + rotates instead of
/// tying up the client connection.
const INLINE_WAIT_MAX_SECS: i64 = 15;
/// How many times one account may be inline-retried on a transient `429` before
/// we give up on it and rotate — bounds a pathological same-account loop.
pub(crate) const MAX_SAME_ACCOUNT_429: u32 = 2;
/// Hold applied to a transient 429 that carries NO retry-after / reset header
/// (σ5, 2026-07-18 live capture: Anthropic burst 429s carry neither). Short so a
/// cold-fan-out that trips every account recovers in seconds instead of the ~60s
/// fleet blackout the old `unwrap_or(60)` default caused. Rotates rather than
/// inline-retrying the limited account.
const NO_GUIDANCE_HOLD_SECS: i64 = 15;
/// Max random jitter (seconds) added to the no-guidance hold: desync the un-park
/// of accounts that tripped together, so a synchronized wave can't re-arm a
/// sliding-window limiter.
pub(crate) const NO_GUIDANCE_JITTER_MAX_SECS: i64 = 5;
/// Ceiling for a quota-rejection hold, mirroring `MAX_RATE_LIMIT_HOLD_SECONDS`
/// in `manager/mod.rs` (which clamps again, so this is the value that actually
/// binds at the call site).
const MAX_QUOTA_HOLD_SECS: i64 = 3600;
/// Spread applied to a CEILING-CLAMPED quota hold so accounts rejected together
/// do not free together. Sized well under the ceiling: it must desync the herd
/// without materially shortening a genuine cap. See [`jittered_quota_hold`].
const QUOTA_HOLD_JITTER_MAX_SECS: i64 = 90;
/// Ceiling for soft-waiting out a *transient* all-parked fleet at the exhausted
/// branch instead of firing the hard 429. Set to the maximum no-guidance park
/// (`NO_GUIDANCE_HOLD_SECS + NO_GUIDANCE_JITTER_MAX_SECS` = 20s): a burst park
/// falls at/under it, while a real quota rejection (`mark_rate_limited` clamped up
/// to 3600s) or a quota window (hours) sits far above it — so real exhaustion is
/// NEVER soft-waited and still hard-fails immediately.
const EXHAUSTION_SOFT_WAIT_MAX_SECS: i64 = NO_GUIDANCE_HOLD_SECS + NO_GUIDANCE_JITTER_MAX_SECS;
// Correctness of the soft-wait path depends on this cap staying BELOW
// `Manager::retry_after_hint`'s `None => 60` sentinel (mod.rs): a soonest-free of
// exactly 60 there means "no account has a known reset", NOT "60s away", so a cap
// >= 60 would make an unknown-reset fleet spuriously soft-wait. Enforced at compile time.
const _: () = assert!(
    EXHAUSTION_SOFT_WAIT_MAX_SECS < 60,
    "soft-wait cap must stay below retry_after_hint's None=>60 sentinel"
);
/// Ceiling on the `retry-after` this proxy ADVERTISES on a synthetic exhausted
/// 429. NOT a ceiling on the wait itself — the body keeps reporting the real
/// figure and the real free-at instant.
///
/// Claude Code refuses anything above 60. Measured 2026-09-15 against a stub
/// endpoint, client 2.1.272: an advertised `60` is honoured (the client sleeps
/// and retries at 60.006s, then succeeds), `61` and every value above it —
/// 65, 75, 90, 120, 300, 57950 — make it give up immediately with
/// `API Error: Request rejected (429)` and exit 1 in about a second. With
/// `CLAUDE_CODE_RETRY_WATCHDOG=1` the same over-ceiling number is worse, not
/// better: the client sleeps it out, so a 16h window-reset figure became a
/// silent multi-hour stall. `exhaustion_hint` times `retry_after` against a
/// quota window's reset instant, which in a real incident was 57,950 to 59,975
/// seconds — i.e. every such 429 was unretryable by construction.
///
/// Why 59 and not the measured 60: `Manager::exhaustion_hint` already uses
/// exactly `60` as its "NO account advertises a reset" sentinel, and the proxy
/// leans on that meaning (see the assertion above). Clamping a measured wait TO
/// 60 would put the sentinel's value on the wire for a known 16-hour window,
/// and nothing downstream — a log line, an operator, a tcr chained behind
/// another tcr through [`parse_retry_after`] — could then tell "we know of no
/// reset" from "we measured a very long one". At 59 the two stay distinct: any
/// advertised value at or below this cap was derived from a real free-at
/// instant, and exactly `60` still means only the sentinel. The cost is one
/// second on a wait whose real length is in the body anyway.
const CLIENT_RETRY_AFTER_MAX_SECS: i64 = 59;
// The advertised cap sits strictly between the soft-wait ceiling and the
// sentinel: above the soft-wait cap, because anything at/below that is absorbed
// in-process and never reaches this header; below 60, for the reason above.
const _: () = assert!(
    EXHAUSTION_SOFT_WAIT_MAX_SECS < CLIENT_RETRY_AFTER_MAX_SECS && CLIENT_RETRY_AFTER_MAX_SECS < 60,
    "advertised retry-after cap must sit between the soft-wait ceiling and the \
     None=>60 sentinel"
);

/// Anthropic's non-standard `529 Overloaded`. Not in [`StatusCode`]'s constants
/// (it is outside the IANA registry), so it is compared as a raw `u16`.
const STATUS_OVERLOADED: u16 = 529;
/// How many times one account may be retried IN PLACE on a `529 Overloaded`
/// before the status is forwarded to the client. Counts RETRIES, not attempts —
/// same semantics as [`MAX_SAME_ACCOUNT_429`] — so the budget is `1 + this`
/// upstream sends on that account.
const MAX_SAME_ACCOUNT_529_RETRIES: u32 = 2;
/// Base of the escalating 529 backoff: retry `n` waits `BASE << n` seconds
/// (1s, 2s), so the no-`retry-after` ladder adds 3s to a request at worst.
const RETRY_529_BASE_BACKOFF_SECS: i64 = 1;
/// Ceiling on ANY single 529 backoff, including a server-supplied `retry-after`.
/// With [`MAX_SAME_ACCOUNT_529_RETRIES`] this bounds the added latency at 8s PER
/// ACCOUNT — deliberately single-digit, because the in-flight guard is held across
/// the wait (see the 529 arm in [`handle`]). A request may spend that ladder on up
/// to `1 + `[`MAX_529_FAILOVERS_PER_REQUEST`] accounts, so the REQUEST-level ceiling
/// is a multiple of it (`overloaded_529_failover_worst_case_latency_is_bounded`).
const RETRY_529_MAX_BACKOFF_SECS: i64 = 4;
/// How many DIFFERENT accounts one request may fail over to after a
/// `529 Overloaded` has spent its IN-PLACE retries on an account.
///
/// The 529 arm shipped deliberately rotation-free, on the premise that a 529 means
/// the SERVER is saturated — so another account would be equally overloaded and the
/// failover would only pay a cold prompt cache. The live log falsifies that: over
/// one 8-minute window a single account answered 136 529s while its siblings served
/// 200s, and a session diverted off it (by an unrelated transport timeout) got a 200
/// from a sibling two seconds later. The overload is ACCOUNT-scoped and
/// time-varying, so one cold prefix is worth trading for a served request.
///
/// Bounded at 2 (at most 3 accounts per request) because the cost is paid in full
/// on every hop: each new account re-runs its own in-place ladder before yielding,
/// so the REQUEST's worst-case added sleep is `1 + this` times the per-account total
/// (asserted in `overloaded_529_failover_worst_case_latency_is_bounded`). Unbounded,
/// one request would walk an overloaded fleet end to end and every concurrent
/// request would do it too.
const MAX_529_FAILOVERS_PER_REQUEST: u32 = 2;

/// How many times ONE client request may wait in place for name resolution to
/// come back before it gives up and answers `503`.
///
/// Per-REQUEST, not per-account, and deliberately small. A DNS failure is a
/// statement about the MACHINE, so there is no fleet to walk and nothing to be
/// gained by spending more of the attempt budget on it — the only question is
/// whether the outage is short enough to ride out without the client noticing.
/// With [`OFFLINE_WAIT_SECS`] this bounds the added latency at 6s per request.
const MAX_OFFLINE_WAITS_PER_REQUEST: u32 = 3;
/// Pause between the in-place retries counted by [`MAX_OFFLINE_WAITS_PER_REQUEST`].
/// Short: a lid-wake resolver comes back within a couple of seconds or not at
/// all, and the per-account in-flight slot is held across the wait.
const OFFLINE_WAIT_SECS: u64 = 2;
/// `retry-after` handed to the client with the offline `503`. The condition is
/// recoverable and usually short — measured 2026-08-10, DNS was back within
/// seconds of each full wake — so this is a nudge to come back, not a park.
const OFFLINE_RETRY_AFTER_SECS: i64 = 5;

/// Backoff (seconds) before the `retried`-th in-place retry of a `529 Overloaded`.
///
/// The ladder is exponential from [`RETRY_529_BASE_BACKOFF_SECS`] — 1s, 2s — so a
/// briefly-overloaded upstream is retried almost immediately while a persistently
/// overloaded one is not hammered. A `retry-after` is HONOURED as a FLOOR (the
/// server knows how long it needs better than the ladder does) but CLAMPED to
/// [`RETRY_529_MAX_BACKOFF_SECS`]: an overloaded upstream asking for 300s must
/// never park a client connection — and the per-account in-flight slot it holds —
/// for minutes. Past the clamp the honest answer is to forward the 529 and let
/// the client decide. Pure — unit-tested.
fn backoff_529_secs(retried: u32, retry_after: Option<i64>) -> u64 {
    // `min(16)` keeps the shift defined for any counter; the clamp below makes
    // every value past the first couple of rungs collapse to the ceiling anyway.
    let ladder = RETRY_529_BASE_BACKOFF_SECS.saturating_mul(1i64 << retried.min(16));
    let secs = match retry_after {
        Some(hint) => hint.max(ladder),
        None => ladder,
    };
    secs.clamp(1, RETRY_529_MAX_BACKOFF_SECS) as u64
}

/// How a transient (non-quota-rejected) 429 should be handled.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Transient429 {
    /// Wait `secs` inline on the same account, then retry it.
    InlineWait(i64),
    /// Park the account for `secs` and route THIS request elsewhere.
    ///
    /// "Rotate" is only half the story for a pinned session. The park arms
    /// `rate_limited_until_ms`, and selection reads its REMAINING duration against
    /// `CACHE_WARM_HOLD_SECS` (`src/manager/mod.rs`): a park that clears while the
    /// prompt cache is still warm diverts this one request and LEAVES THE PIN, so
    /// the session comes home to its warm prefix when the timer runs out; only a
    /// park that outlives the cache re-keys the session durably. Every park this
    /// function produces is short — `NO_GUIDANCE_HOLD_SECS` + jitter, or a
    /// `retry-after` clamped to 300s — so the pin-keeping branch is the common one
    /// here. The long parks come from the quota-rejected path instead, which calls
    /// `mark_rate_limited` with a `retry-after` clamped up to 3600s.
    Park(i64),
}

/// Decide how to handle a transient (non-quota-rejected) 429. Pure — unit-tested.
///
/// A PRESENT `retry-after` keeps the historical semantics (inline-wait a short
/// hint, else park+rotate). An ABSENT header no longer fabricates a 60s park
/// (which blacked out the whole fleet on a cold fan-out); it parks a short
/// [`NO_GUIDANCE_HOLD_SECS`] and rotates so the next probe can discover the real
/// window instead of the account inline-retrying into its own limit.
///
/// `jitter` (seconds) is added ONLY to the no-guidance hold so accounts that
/// tripped together un-park at staggered times; the present-`retry-after` path
/// The hold to arm for a QUOTA-REJECTED 429, jittered so a burst of accounts
/// rejected together does not un-park in lockstep.
///
/// Why this exists, measured 2026-08-04: a fan-out drew 52 rate-limit events in
/// 72 minutes, and **42 of them armed an identical 3600s hold** — eight accounts
/// inside a four-second window. They would then all have freed within that same
/// four seconds and re-burst as one wave. [`NO_GUIDANCE_JITTER_MAX_SECS`] already
/// documents this hazard verbatim ("desync the un-park of accounts that tripped
/// together, so a synchronized wave can't re-arm a sliding-window limiter") but
/// was only ever wired to the no-guidance path, never to this one.
///
/// The jitter is SUBTRACTED, and only from a hold that hit the ceiling. That is
/// deliberate and is the only direction that is safe here:
///
/// * Adding is useless — [`Manager::mark_rate_limited`] clamps to
///   `MAX_RATE_LIMIT_HOLD_SECONDS` (3600), so `3600 + jitter` clamps straight
///   back to 3600 and the herd stays synchronized. The 42 identical holds were
///   synchronized by OUR ceiling, not by upstream's number.
/// * Subtracting from a SHORT hold would return an account before upstream said
///   it may come back — the one thing this path must never do. So a hold under
///   the ceiling is armed verbatim, unjittered.
/// * At the ceiling we are ALREADY returning earlier than asked (upstream wanted
///   `3600s` or more and got exactly 3600), so spreading within
///   `[3600 - N, 3600]` stays inside an envelope we had already chosen, and
///   strictly improves on arming every account at one instant.
pub(crate) fn jittered_quota_hold(retry_after: i64, nanos: u32) -> i64 {
    let clamped = retry_after.clamp(1, MAX_QUOTA_HOLD_SECS);
    if clamped < MAX_QUOTA_HOLD_SECS {
        return clamped;
    }
    let jitter = i64::from(nanos) % (QUOTA_HOLD_JITTER_MAX_SECS + 1);
    (clamped - jitter).max(1)
}

/// ignores it and stays byte-identical to the historical behavior.
pub(crate) fn classify_transient_429(
    retry_after: Option<i64>,
    retried: u32,
    jitter: i64,
) -> Transient429 {
    match retry_after {
        Some(secs) => {
            let wait = secs.clamp(1, 300);
            if retried < MAX_SAME_ACCOUNT_429 && wait <= INLINE_WAIT_MAX_SECS {
                Transient429::InlineWait(wait)
            } else {
                Transient429::Park(wait)
            }
        }
        None => Transient429::Park(NO_GUIDANCE_HOLD_SECS + jitter),
    }
}

/// Whether an upstream `429` refused the REQUEST rather than the account, so it is
/// forwarded to the client as-is instead of parking the account and rotating.
///
/// True only when both hold:
/// - upstream gave no quota guidance at all: no `unified-*` rejection, no
///   `retry-after` header, no `anthropic-ratelimit-unified-status` header;
/// - the request lacks `?beta=true`, which Claude Code's own client puts on every
///   `/v1/messages` call (10,575 of 10,575 on 2026-09-24).
///
/// Measured over this proxy's log for 2026-09-20 to 2026-09-24: all 1,704 upstream
/// 429s on `/v1/messages` without `?beta=true` carried no guidance, and an account
/// that refused one such request served the same client's next request 0.4 s
/// later. The refusal followed the request, not the account. Each of those 429s
/// used to park the account for 15 to 20 s and rotate to the next, so one direct
/// API client walked the fleet into "All 18 accounts exhausted" for every Claude
/// Code session behind it.
///
/// Claude Code traffic also draws no-guidance 429s (93 in the same window) and
/// keeps the park-and-rotate handling: nothing measured says those are about the
/// request.
pub(crate) fn is_request_scoped_429(
    path_and_query: &str,
    up_headers: &HeaderMap,
    quota_rejected: bool,
) -> bool {
    let guided = quota_rejected
        || up_headers.contains_key("retry-after")
        || up_headers.contains_key("anthropic-ratelimit-unified-status");
    !guided && !carries_beta_flag(path_and_query)
}

/// Whether the query string has a `beta=true` pair, the mark Claude Code's client
/// (the Anthropic SDK's beta namespace) puts on every Messages call.
fn carries_beta_flag(path_and_query: &str) -> bool {
    path_and_query
        .split_once('?')
        .is_some_and(|(_, query)| query.split('&').any(|pair| pair == "beta=true"))
}

/// Decide whether an all-accounts-unavailable state is a *transient* fleet-park
/// worth soft-waiting (Some(secs) to sleep) vs genuine exhaustion (None → hard 429).
/// `soonest_free_secs` is `Manager::retry_after_hint`; `already_waited` caps us to
/// ONE soft-wait per client request. Real exhaustion has `soonest_free` far above the
/// transient ceiling → None. Kept pure so it is unit-tested without timing.
const fn soft_wait_secs(soonest_free_secs: i64, already_waited: bool) -> Option<u64> {
    if already_waited || soonest_free_secs <= 0 || soonest_free_secs > EXHAUSTION_SOFT_WAIT_MAX_SECS
    {
        None
    } else {
        Some(soonest_free_secs as u64)
    }
}

/// Frames in a transport error's `source()` chain that name a NAME-RESOLUTION
/// failure. Matched case-insensitively as substrings of each frame's `Display`.
///
/// The first is hyper-util's own label for the connector's DNS phase
/// (`ConnectError::new("dns error", ..)`); the rest are the operating system's
/// `getaddrinfo` texts that `std` wraps into an `io::Error` whose `ErrorKind` is
/// `Uncategorized` — a kind that cannot be matched on stable Rust, which is why
/// this is a string match and not a `downcast_ref` + kind comparison.
const DNS_FAILURE_MARKERS: [&str; 4] = [
    "dns error",
    "failed to lookup address information",
    "nodename nor servname provided",
    "temporary failure in name resolution",
];

/// Is this transport failure the machine being OFFLINE rather than a bad route
/// to one upstream?
///
/// Why it is not `err.is_connect()`: `is_connect()` is true for a DNS failure
/// AND for a refused/blackholed connection to a host that resolves fine. Only
/// the first is machine-level. Every account in the fleet resolves the SAME
/// hostname, so a resolver failure is not evidence about any account — answering
/// it by rotating walks all thirteen accounts in under a second, unpins the
/// session (a re-pin costs a full cold prefix, the most expensive event here) and
/// hands the client a 502. Measured 2026-08-10: 57 client-visible 502s, all
/// inside one lid-close/dark-wake window, 39-159 resolver failures per minute.
/// A refused connection to a reachable host IS real evidence about a route, so
/// it must keep taking the rotate arm.
///
/// The chain must be WALKED. `reqwest::Error`'s `Display` prints only its own top
/// frame and never recurses (the same property that forces `?err` over `%err` at
/// the log site below), so the resolver text lives strictly in a `source()`.
pub(crate) fn is_offline_error(err: &reqwest::Error) -> bool {
    let mut frame: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(cause) = frame {
        let rendered = cause.to_string().to_ascii_lowercase();
        if DNS_FAILURE_MARKERS
            .iter()
            .any(|marker| rendered.contains(marker))
        {
            return true;
        }
        frame = cause.source();
    }
    false
}

/// Which failure the transport-failure arm is actually looking at, decided
/// ONCE per attempt rather than re-derived at each log/retry/recycle site.
///
/// The distinction this exists to draw: does the pooled connection survive
/// the error, or not? A CONNECT-phase failure never had a connection to
/// survive. Past connect, reqwest evicts a connection-level death (the socket
/// itself closed) from its pool as part of raising the error, so the next
/// pooled checkout is fresh by construction. An HTTP/2 stream reset is
/// neither: the h2 CONNECTION stays open and pooled (only the one stream
/// failed), so every retry and every rotation checks out the SAME broken
/// connection until something explicitly recycles it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransportFailureReason {
    /// No connection existed yet (`err.is_connect()`).
    ConnectPhase,
    /// Past connect, and the source chain names an HTTP/2 stream reset, the
    /// connection is still pooled and must be recycled before a retry means
    /// anything.
    StreamReset,
    /// Past connect, not a recognized stream reset: a genuine connection-level
    /// death, already evicted from the pool by the error itself.
    Other,
}

/// Substrings of an h2 stream-reset error's rendered `Display`, matched
/// case-insensitively across the walked `source()` chain.
///
/// The h2 crate is only a transitive dependency here (pulled in through
/// hyper), so its `h2::Error` type is not nameable from this crate without
/// adding a new direct dependency, which this fix does not do. The h2 error
/// type IS reachable in principle (`std::error::Error::source()` on
/// `hyper::Error` returns it as a `dyn Error`), but without the crate in
/// scope there is nothing to `downcast_ref` to. So this matches the rendered
/// text instead, the same way [`is_offline_error`] above matches DNS text it
/// cannot downcast to either.
///
/// Measured against the real crate (h2 0.4.16, `src/error.rs`), not guessed:
/// the incident log's `Reset(StreamId(n), PROTOCOL_ERROR, Remote)` is the
/// error's `Debug`, and `Reason::PROTOCOL_ERROR`'s own `Display` is the prose
/// "unspecific protocol error detected", neither `"protocol_error"` nor
/// `"reset("` ever appears in what `source()` actually renders, so matching
/// either would silently never fire. What IS reason-independent and always
/// present: `Error`'s `Display` impl hard-codes the literal prefix
/// `"stream error "` for a `Kind::Reset` regardless of which of the three
/// `Initiator`s it names ("stream error sent by user: …", "stream error
/// detected: …", "stream error received: …") and regardless of the reset
/// reason, while every other `Kind` (`GoAway`, `Reason`, `User`, `Io`) always
/// renders `"connection error …"` or something with neither phrase. One
/// marker, not per-reason string matching.
const H2_STREAM_RESET_MARKERS: [&str; 1] = ["stream error "];

/// Classify a transport failure into [`TransportFailureReason`], walking the
/// same `source()` chain [`is_offline_error`] walks, for the same reason: the
/// h2 sub-kind lives strictly in a `source()` frame, never in `reqwest::Error`'s
/// own `Display`.
pub(crate) fn classify_transport_failure(err: &reqwest::Error) -> TransportFailureReason {
    if err.is_connect() {
        return TransportFailureReason::ConnectPhase;
    }
    let mut frame: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(cause) = frame {
        let rendered = cause.to_string().to_ascii_lowercase();
        if H2_STREAM_RESET_MARKERS
            .iter()
            .any(|marker| rendered.contains(marker))
        {
            return TransportFailureReason::StreamReset;
        }
        frame = cause.source();
    }
    TransportFailureReason::Other
}

/// How many times one account may be retried IN PLACE on a non-CONNECT
/// transport failure before the request gives up on it and rotates. Counts
/// RETRIES, not attempts, matching [`MAX_SAME_ACCOUNT_429`].
///
/// Was effectively `1` (a `HashSet` membership test) until 2026-08-27, on the
/// theory that "the second failure IS evidence about the account". Live
/// measurement falsified that: over one day's log, the FIRST same-account retry
/// landed a `200` in **94.7%** of cases (1286 of 1358) — the fault is the pooled
/// h2 connection aging out, not the account. The cases where one retry was not
/// enough rotated a warm session onto a cold account 841 times that day, and a
/// rotation costs a full prompt-cache miss (measured 5-10x the quota of a warm
/// serve) to escape a fault a second reconnect clears.
///
/// Why 3 and not 2 or 10: the 94.7% is measured for the FIRST retry only, and
/// whether later retries clear at the same rate is NOT measured — a run of
/// failures on one aging pool is plausibly correlated rather than independent.
/// So this is sized as a small ladder that covers a short burst without
/// pretending to a compounding-probability argument the data does not support.
/// If the live `transport_retry=` field shows attempts 2 and 3 rarely helping,
/// lower it; if rotations persist at 3, the ceiling is not the binding problem.
///
/// The ceiling still exists because a genuinely broken account must not spin
/// here forever; it is sized to make cache-preserving recovery the common path,
/// not to make rotation unreachable.
const MAX_SAME_ACCOUNT_TRANSPORT_RETRIES: u32 = 3;

/// The most upstream sends ONE account can absorb inside a single client
/// request, derived from the per-account ladders rather than guessed.
///
/// Every term is a counter in [`handle`] that is monotone per account, so this
/// is a real ceiling and not an estimate:
///
/// * `1` — the send itself.
/// * [`MAX_SAME_ACCOUNT_TRANSPORT_RETRIES`] — same-account transport retries
///   (`transport_retried`).
/// * `1` — the 401 force-refresh retry (`forced_401`).
/// * [`MAX_SAME_ACCOUNT_429`] — transient-429 inline waits (`retried_429`).
/// * [`MAX_SAME_ACCOUNT_529_RETRIES`] — 529 in-place backoffs (`retried_529`).
const MAX_SENDS_PER_ACCOUNT: usize = 2
    + MAX_SAME_ACCOUNT_TRANSPORT_RETRIES as usize
    + MAX_SAME_ACCOUNT_429 as usize
    + MAX_SAME_ACCOUNT_529_RETRIES as usize;

/// The rotation loop's TOTAL attempt budget for a fleet of `account_count`: the
/// per-account ladder ceiling times the fleet, plus a small constant for the
/// iterations that consume a turn without sending (the one-shot exhaustion
/// soft-wait). Bounds [`handle`]'s loop without ever truncating a walk the
/// ladders themselves permit.
///
/// It used to be `2n + 4` — "two sends per account plus a small constant" — which
/// stopped being true when the same-account transport retry added a third
/// potential send. The failure was silent and specifically MIXED: a blip on one
/// account plus the 529 ladder on the next spends 4 sends per account, so a
/// 3-account walk needs 12 against a budget of 10. The loop fell out mid-walk,
/// `every_attempt_transport_failed` was false, and the client got a SYNTHESIZED
/// 429 ("all accounts exhausted", with a fabricated retry-after) in place of the
/// real 529 — or of a 200 from an account that was never tried.
///
/// Every ladder in the sum is independently bounded and each rung is capped
/// (`RETRY_529_MAX_BACKOFF_SECS`, `INLINE_WAIT_MAX_SECS`), so widening this does
/// not widen any latency ceiling; it removes a truncation, not a guard.
///
/// Extracted from the loop so the headroom assertions in
/// `overloaded_529_failover_worst_case_latency_is_bounded` and
/// `the_mixed_transport_and_529_ladder_fits_the_attempt_budget` bind THIS
/// formula instead of a copy of it that could silently drift from it.
///
/// [`MAX_OFFLINE_WAITS_PER_REQUEST`] is added on top because the offline arm's
/// in-place retries are bounded per REQUEST, not per account, so they are not
/// expressible in [`MAX_SENDS_PER_ACCOUNT`]. Without the term a request that
/// rides out a resolver blip could be truncated mid-walk — the exact silent
/// failure the `2n + 4` formula caused.
fn max_attempts_for(account_count: usize) -> usize {
    account_count
        .saturating_mul(MAX_SENDS_PER_ACCOUNT)
        .saturating_add(4 + MAX_OFFLINE_WAITS_PER_REQUEST as usize)
        .max(1)
}

/// The client's socket address, injected into request extensions by the hybrid
/// server ([`crate::mitm::serve_http`]) so the auth layer can exempt loopback
/// clients from the api-key gate. A localhost personal proxy must not demand its
/// own key from the local user; a non-loopback client (were the listener ever
/// bound wider than 127.0.0.1) still presents it: a loopback client is exempt.
#[derive(Clone, Copy)]
pub struct ClientAddr(pub SocketAddr);

/// The per-connection session key, injected into request extensions by the hybrid
/// server ([`crate::mitm::serve_http`]) **only when session affinity is enabled**.
/// Its PRESENCE is the feature flag: it lets a request derive a pin via
/// [`stable_session_key`] and pass it as [`Manager::select`]'s `affinity` arg; its
/// absence (the default) leaves selection at the per-request LRU rotation. The
/// wrapped value is deliberately NOT used as a routing key — one connection is one
/// `claude` process, but a pin keyed on it dies with the connection and leaves a
/// ghost entry behind, so a request with no stable identity routes unpinned.
#[derive(Clone, Copy)]
pub struct SessionKey(pub u64);

/// The host of the `CONNECT` tunnel a request came through, injected into request extensions by
/// the hybrid server (`crate::mitm::serve_http`) on every request it decrypts from a tunnel it
/// terminated, and never on a base-URL request. A client cannot set an extension, so its
/// presence means the request came through a tunnel to this host, whatever `Host` the request
/// declares, and `handle`'s host guard holds the request to it.
#[derive(Clone)]
pub struct InterceptedHost(pub String);

/// Stamped beside [`InterceptedHost`] when the tunnel's `CONNECT` carried the request key
/// ([`crate::request_key::ProxyCredential`]). Like that one, no client can set it, so a request
/// decrypted from a keyed tunnel is keyed without its headers being read again.
#[derive(Clone, Copy)]
pub struct TunnelKeyed;

/// How one request stands against the request key, decided from what the listener stamped
/// ([`TunnelKeyed`], [`InterceptedHost`]) before any header is read: a request decrypted from a
/// tunnel was judged at its `CONNECT`, so only a plain request is parsed
/// ([`crate::request_key::ProxyCredential::from_request_headers`]).
fn request_keying(
    extensions: &axum::http::Extensions,
    headers: &HeaderMap,
    request_key: Option<&str>,
) -> crate::request_key::RequestKeying {
    use crate::request_key::{ProxyCredential, RequestKeying};
    if request_key.is_none() {
        return RequestKeying::NoServerKey;
    }
    if extensions.get::<TunnelKeyed>().is_some() {
        return RequestKeying::Keyed;
    }
    if extensions.get::<InterceptedHost>().is_some() {
        return RequestKeying::CountedAtConnect;
    }
    let credential = ProxyCredential::from_request_headers(headers, request_key);
    if credential.key_ok {
        RequestKeying::Keyed
    } else {
        RequestKeying::Unkeyed(credential.user_agent)
    }
}

/// A deterministic hash of `prefix` + `value`, used to derive a stable affinity
/// key. What this needs to hold is DETERMINISM ACROSS PROCESSES, not just
/// within one — pins now persist to disk and are restored at boot (see
/// [`crate::affinity`]), so a hash that only agreed with itself for the
/// lifetime of one process would silently stop matching every restart. That
/// property is documented, measured and cited at [`crate::affinity::StoredPin::key`]
/// — read it there rather than re-deriving it here.
fn stable_hash(prefix: &str, value: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    prefix.hash(&mut hasher);
    value.hash(&mut hasher);
    hasher.finish()
}

/// Tier 3 of [`stable_session_key`]: derive a fallback affinity key from the
/// request's cacheable prefix, the exact text of its `system` and `tools` in the
/// forwarded body ([`crate::request_body::RequestBody::affinity_prefix`]), when the client carries no stable identity at
/// all. Returns `None` when BOTH are absent — there is then no cacheable
/// prefix, so a pin buys no cache win and only concentrates unrelated
/// anonymous traffic onto one account. This is the guard that stops every
/// trivial anonymous request (no system prompt, no tools) from piling onto a
/// single account; do not drop it.
///
/// Hashes the RAW bytes of each field, exactly as they sit in the body — never
/// canonicalized — namespaced under `"pfx:"` so this input space cannot collide
/// with the `"sid:"` / `"key:"` / `"uid:"` tiers. `system` and `tools` are hashed as
/// `Option<&str>` (not concatenated into one string) so presence/absence of
/// each field is part of the hash input and a `(None, Some("ab"))` pair can
/// never collide with a `(Some("ab"), None)` pair; `str`'s own `Hash` impl
/// appends a sentinel byte after each value, so this is also safe against a
/// `system`/`tools` boundary shift (e.g. `("a", "bc")` vs `("ab", "c")`).
///
/// Deliberately NO minimum-size floor. A tiny `system`/`tools` field that can
/// never reach Anthropic's minimum cacheable-prefix length pins for zero cache
/// benefit — a floor would fix that. But the floor would have to be a BYTE
/// count on the raw JSON text, while Anthropic's minimum is a TOKEN count on
/// tokenized content, and this proxy has no tokenizer: it never decodes model
/// text, only relays bytes. Any byte→token constant here would be an
/// undefended guess — wrong in one direction wastes the tier's whole purpose,
/// wrong in the other pins traffic that was never going to hit cache. Omitted;
/// revisit only with either a real tokenizer in this process or a measured
/// bytes-per-token ratio from live traffic to ground the constant.
fn prefix_session_key(system: Option<&str>, tools: Option<&str>) -> Option<u64> {
    if system.is_none() && tools.is_none() {
        return None;
    }
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    "pfx:".hash(&mut hasher);
    system.hash(&mut hasher);
    tools.hash(&mut hasher);
    Some(hasher.finish())
}

/// The conversation a Codex request names in its `session-id` header, trimmed; `None` when
/// the header is absent, not ASCII, or blank. One reader for the two places that key on it:
/// session affinity's tier 0 ([`stable_session_key`]) and the wire session `handle` records.
fn codex_session_id(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(crate::provider::CODEX_SESSION_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Derive a STABLE affinity key from the client's most durable identity, so a
/// session survives reconnects on one account (warm prompt cache). Priority:
///
///   0. for a CODEX request only (decided by `path`, see
///      [`crate::provider::Provider::of_path`]), the `session-id` header
///      ([`crate::provider::CODEX_SESSION_HEADER`]) Codex sends on every turn.
///      It names one conversation, which is exactly the grain a pin wants, and
///      it is readable where the body is not: a Codex body is usually
///      zstd-compressed, which tiers 2 and 3 cannot parse. It outranks tier 1
///      for a loopback AND a remote client — an `x-api-key` names at best one
///      client, never one conversation, and one key shared by N sessions must
///      not collapse them onto one account (the reason tier 2 outranks tier 1
///      on loopback). A missing, non-ASCII, empty or whitespace-only header
///      falls through to the tiers below unchanged, and an Anthropic request
///      never reads it: a stray `session-id` on `/v1/messages` keys nothing
///      (the `openai/codex` citations are in the function body). Else
///   1. the `x-api-key` header (distinct team keys → distinct accounts) — but
///      SKIPPED when it equals the configured proxy key, since the shared proxy
///      secret is not a per-client identity (every remote client sends it), and
///      DEMOTED below tier 2 for a LOOPBACK client, whose key is whatever its
///      launcher exported and may be one placeholder shared by every process on
///      the box (the 2026-09-22 measurement is in the function body), else
///   2. the body's top-level `metadata.user_id` (the loopback/personal path,
///      where there's no distinguishing x-api-key). Claude Code sends this as a
///      STRINGIFIED JSON blob `{"device_id":"…","account_uuid":"…","session_id":"…"}`.
///      Live-verified 2026-07-16 (6-request header/body capture): the embedded
///      `session_id` is LINEAGE-stable — a resumed session (`-r <id>`) keeps the
///      ORIGINAL id here, and subagent/sidechain requests carry the PARENT's id —
///      so hashing the whole string pins a conversation, its resumes, and its
///      subagents to one account (warm prompt cache). Do NOT key on the
///      `x-claude-code-session-id` HEADER instead: it forks on resume and varies
///      across sidechain requests, which would cold-start the cache. Else
///   3. a hash of the request's cacheable prefix (`system` + `tools`, see
///      [`prefix_session_key`]) — weaker than 1/2 because it says nothing about
///      the CLIENT, only about what this one request would cache identically
///      with another, so it is scoped MUCH more narrowly than tiers 1/2. Still
///      routes reconnects of an SDK/`curl` caller with no identity onto the
///      same account instead of cold-starting every request, and self-balances
///      because distinct prefixes hash to distinct accounts. Else
///   4. `None` — no stable identity and no in-scope cacheable prefix, so the
///      request routes UNPINNED (plain LRU). It deliberately does NOT fall
///      back to the per-connection [`SessionKey`]: that mints a pin no
///      reconnect can ever reuse or reclaim.
///
/// Tier 3's scope, and why both halves are required:
///
/// - **`POST /v1/messages` only**, exact match on `path`, not a prefix match —
///   `/v1/messages/count_tokens` must NOT qualify. Anthropic documents that
///   token counting never uses prompt caching, so pinning it would concentrate
///   load for zero cache benefit. `path` is the caller's ALREADY
///   query-stripped path — see `handle`, which matches the same guards this
///   way.
/// - **Loopback callers only**, via `client_is_loopback` (the same
///   [`ClientAddr`] extension `handle`'s api-key gate uses). Tier 1 already
///   refuses to key on `x-api-key` when it equals the configured proxy secret,
///   specifically so remote clients sharing that secret don't collapse onto
///   one account (see the comment below). Tier 3 has no such secret to check —
///   it keys off the request BODY — so without this gate N remote workers
///   sharing one harness (one system prompt) would collapse onto one account
///   through the exact back door tier 1 closes. A loopback proxy serves one
///   real user; a non-loopback deployment may not. (`IpAddr::is_loopback`
///   does not recognize an IPv4-mapped IPv6 peer like `::ffff:127.0.0.1` as
///   loopback, so such a peer reads as remote and fails CLOSED here — the
///   safe direction, and the same primitive [`ClientAddr`]'s own doc-comment
///   and the api-key exemption already accept this same way.)
///
/// Returns `None` on absence/parse failure at every tier, which routes the
/// request unpinned. The paired [`SessionKind`] records WHICH tier produced the
/// key — display provenance only, never a routing input.
fn stable_session_key(
    headers: &HeaderMap,
    body: &crate::request_body::RequestBody,
    secrets: ProxySecrets<'_>,
    method: &Method,
    path: &str,
    client_is_loopback: bool,
) -> Option<(u64, SessionKind)> {
    // Tier 0 (Codex only): the `session-id` header Codex sends on every turn
    // (`openai/codex` `codex-api/src/requests/headers.rs:8`). It is per conversation, which is
    // exactly what a pin wants, and it is readable where the body may not be: the body is
    // usually zstd-compressed (`core/src/client.rs:1704-1711`), and then tier 2 cannot parse
    // it. (Tier 3 never applies to a Codex path at all: its exact `/v1/messages` check below
    // excludes it.) It outranks tier 1's `x-api-key` for a loopback AND a remote client. Tier 1
    // already discards the shared proxy secret; any other key names at best one client, never
    // one conversation, and one key shared by N sessions must not collapse them onto one
    // account, the same reason `metadata.user_id` outranks it on loopback.
    match Provider::of_path(path) {
        Provider::Codex => {
            if let Some(sid) = codex_session_id(headers) {
                return Some((stable_hash("sid:", sid), SessionKind::Stable));
            }
        }
        Provider::Anthropic => {}
    }

    // A proxy secret (`proxy.apiKey` or the request key) is not a client identity —
    // skip it so clients sharing it don't all collapse onto one account.
    let api_key = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .filter(|key| !secrets.contains(Some(key)))
        .map(|key| (stable_hash("key:", key), SessionKind::Stable));

    // A REMOTE client's key is its identity (tier 1 first). On LOOPBACK the
    // key is whatever the wrapper that launched `claude` exported, and the
    // api-key gate never checked it, so it may be one placeholder shared by
    // every process on the box: measured 2026-09-22, 34 `claude` processes
    // under one `ANTHROPIC_API_KEY=via-tcr` collapsed onto ONE session key,
    // 14,628 requests routed as one conversation, one account run to 100% of
    // its window and the next one handed advisor results minted on the first.
    // So on loopback the body's per-conversation `metadata.user_id` outranks
    // the header, and the header keys only a client that sends no `user_id`.
    if !client_is_loopback && let Some(key) = api_key {
        return Some(key);
    }

    // Tier 2 (`metadata.user_id`) and tier 3 (`system`/`tools`) read the request's
    // one parse; see [`crate::request_body`].
    if let Some(user_id) = body.affinity_user_id() {
        return Some((stable_hash("uid:", user_id), SessionKind::Stable));
    }
    if let Some(key) = api_key {
        return Some(key);
    }

    // Tier 3's scope guard — see the doc-comment above for why both halves are
    // load-bearing. `path` must match EXACTLY: `/v1/messages` and nothing
    // longer (`/v1/messages/count_tokens`), nothing shorter.
    if !client_is_loopback || *method != Method::POST || path != "/v1/messages" {
        return None;
    }
    let (system, tools) = body.affinity_prefix()?;
    prefix_session_key(system, tools).map(|key| (key, SessionKind::Prefix))
}

/// Path of the read-only live-status endpoint [`status_handler`] serves.
///
/// The `_tcr/` prefix is what makes the route safe to add to a transparent
/// forwarder: every path a client legitimately means for Anthropic lives under
/// `/v1/…` (`/v1/messages`, `/v1/models`, `/v1/organizations/…`), so a segment
/// starting with an underscore can neither collide with a request we must forward
/// today nor be shadowed by an Anthropic route added tomorrow. Anything the proxy
/// ever answers locally belongs under this prefix.
pub const STATUS_PATH: &str = "/_tcr/status";

/// Path of the peer dial route [`peer_dial_handler`] serves: the CLI asks the
/// running tcr to dial a tailnet address through tcr's own tailnet node, which
/// lives in this process, and gets the connection back as a byte stream.
/// `POST` with `Upgrade: tcr-peer-dial`, behind [`local_endpoint_gate`].
pub const PEER_DIAL_PATH: &str = "/_tcr/peer/dial";

/// The `Upgrade` protocol token [`PEER_DIAL_PATH`] answers `101` to.
pub const PEER_DIAL_UPGRADE: &str = "tcr-peer-dial";

/// The request header naming the socket [`PEER_DIAL_PATH`] dials.
pub const PEER_DIAL_TARGET_HEADER: &str = "x-tcr-dial";

/// The [`ENDPOINT_HEADER`] value the peer dial route stamps.
pub const PEER_DIAL_ENDPOINT: &str = "peer-dial";

/// Path of the light quota read [`quota_handler`] serves, for a status line that
/// ticks every turn: per account its gate and its 5-hour and weekly utilization,
/// read from memory. `GET` only, behind the same gate as [`STATUS_PATH`].
pub const QUOTA_PATH: &str = "/_tcr/quota";

/// Path of the routing document [`routing_handler`] serves ([`crate::routing`], schema 1):
/// which model families each provider pool can serve a new request for now, for a launcher
/// that must not parse text. `GET` only, behind the same gate as [`STATUS_PATH`].
pub const ROUTING_PATH: &str = "/_tcr/routing";

/// Path of the live account-control endpoint [`set_disabled_handler`] serves —
/// `POST` only, the one route on this process that MUTATES rotation.
///
/// It exists because `disabled` was, until now, a boot-time read: `tcr disable`
/// wrote the config file and the running proxy never re-read it, so an account the
/// operator had parked kept being handed live traffic while every surface reported
/// it benched. The TUI's `d` key was the only correct path, and a `--headless`
/// proxy has no TUI.
///
/// Under [`LOCAL_PREFIX`] for the reason given there — but what protects a
/// MUTATING verb is not the prefix guard, whose reach is only the exact spelling.
/// It is that this path is REGISTERED as a real route, so axum matches it exactly
/// and matches it BEFORE the catch-all: a `POST` here can never be rewritten onto
/// api.anthropic.com carrying a pooled OAuth Bearer, and a near-miss spelling can
/// never land ON the handler. What authorizes the mutation itself is
/// [`local_endpoint_gate`] plus the `application/json` requirement in
/// [`set_disabled_handler`], neither of which the path shape has any part in.
pub const DISABLED_PATH: &str = "/_tcr/accounts/disabled";

/// Marks every response produced by the account-control route itself, so a caller
/// can tell "this proxy has no such route" from "the route answered and said no".
///
/// Both are a 404: a tcr too old to have the route hits the [`LOCAL_PREFIX`]
/// catch-all guard (or, older still, gets Anthropic's own 404 back), and a query
/// naming no account is a 404 from the handler. The CLI's two reactions are
/// opposite — write the file and warn loudly that the live proxy is stale, versus
/// report a bad query — so the distinction cannot rest on matching an error
/// string. This header is the structural discriminator. It is a response header,
/// never a request one, so it authorizes nothing.
pub const ENDPOINT_HEADER: &str = "x-tcr-endpoint";

/// The [`ENDPOINT_HEADER`] value the account-control route stamps.
pub const DISABLED_ENDPOINT: &str = "accounts-disabled";

/// Path of the live account-ADD endpoint [`add_account_handler`] serves — `POST`
/// only, the other route on this process that MUTATES rotation.
///
/// It exists because adding an account has, until now, required stopping the
/// proxy, running `tcr login`, and starting it again — which wipes the in-memory
/// session→account pin map, so every live session cold-starts its prompt cache
/// on the next turn. That is the most expensive event in this system; see
/// [`Manager::add_account`] for the append primitive this route drives and why
/// it must never insert anywhere but the end.
///
/// Same shape as [`DISABLED_PATH`] and for the same reasons: registered as a
/// real route so axum matches it exactly and BEFORE the catch-all — a `POST`
/// here can never be rewritten onto api.anthropic.com carrying a pooled OAuth
/// Bearer. What authorizes the mutation is [`local_endpoint_gate`] plus the
/// `application/json` requirement, neither of which the path shape has any part
/// in.
pub const ADD_ACCOUNT_PATH: &str = "/_tcr/accounts";

/// The [`ENDPOINT_HEADER`] value [`add_account_handler`] stamps. Distinct from
/// [`DISABLED_ENDPOINT`] so a caller can tell which of the two mutating routes
/// answered — the same structural discriminator [`ENDPOINT_HEADER`] documents.
pub const ADD_ACCOUNT_ENDPOINT: &str = "accounts-add";

/// Response header naming, comma-separated, every provider whose accounts this
/// server can store: [`crate::provider::Provider::as_str`] over
/// [`crate::provider::Provider::ALL`], so `anthropic,codex`. Set by
/// `stamp` (for `ADD_ACCOUNT_ROUTE`), so it rides on every answer the account-add route
/// stamps — the blank-probe 400 `login::probe_add_route` provokes, a 405, a
/// 409, a 200 — and never on the unstamped gate refusal, which must not confirm
/// the route exists to a caller that failed the gate.
///
/// It exists because a tcr built before accounts had a `provider` drops that key
/// from a Codex add and stores the credential as a Claude account; from then on
/// the OpenAI tokens go to Anthropic. So a client adding an account of any other
/// provider must see that provider named here before it posts. An Anthropic add
/// needs no such proof, and the header's absence changes nothing for one.
///
/// A SECOND header, deliberately, never a new value of [`ENDPOINT_HEADER`]:
/// `live_client::post_add_account` recognises the route by that stamp's exact value, and
/// every older CLI (TcrBar's bundled one included) would read a changed stamp as
/// "no such route" and refuse every Anthropic login.
pub const PROVIDERS_HEADER: &str = "x-tcr-providers";

/// [`PROVIDERS_HEADER`]'s value: every provider this build can store.
fn advertised_providers() -> String {
    Provider::ALL
        .iter()
        .map(|provider| provider.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

/// The providers a [`PROVIDERS_HEADER`] value names. A name this build does not
/// know is skipped (a newer server may store more), so an absent or unreadable
/// header is no proof of anything, never a provider.
pub fn providers_in_header(value: &str) -> Vec<Provider> {
    value
        .split(',')
        .filter_map(|name| Provider::from_name(name.trim()))
        .collect()
}

/// Path of the control-account endpoint [`set_control_handler`] serves — `POST`
/// only, the third route on this process that MUTATES rotation state.
///
/// Same shape as [`DISABLED_PATH`] and for the same reasons: registered as a
/// real route so axum matches it exactly and BEFORE the catch-all, so a `POST`
/// here can never be rewritten onto api.anthropic.com carrying a pooled OAuth
/// Bearer. What authorizes the mutation is [`local_endpoint_gate`] plus the
/// `application/json` requirement, neither of which the path shape has any
/// part in.
pub const CONTROL_PATH: &str = "/_tcr/accounts/control";

/// The [`ENDPOINT_HEADER`] value [`set_control_handler`] stamps. Distinct from
/// [`DISABLED_ENDPOINT`]/[`ADD_ACCOUNT_ENDPOINT`] so a caller can tell which of
/// the three mutating routes answered.
pub const CONTROL_ENDPOINT: &str = "account-control";

/// Path of the live priority endpoint [`set_priority_handler`] serves, `POST`
/// only, the fourth route on this process that changes rotation state.
///
/// It exists because `priority` was a boot-time read: `tcr priority` wrote the
/// config file and the running proxy kept ranking by the old value until it
/// restarted, which is the cold-cache event this tool exists to avoid.
///
/// Same shape as [`DISABLED_PATH`] and for the same reasons: registered as a
/// real route so axum matches it exactly and BEFORE the catch-all, so a `POST`
/// here can never be rewritten onto api.anthropic.com carrying a pooled OAuth
/// Bearer. What authorizes the change is [`local_endpoint_gate`] plus the
/// `application/json` requirement.
pub const PRIORITY_PATH: &str = "/_tcr/accounts/priority";

/// The [`ENDPOINT_HEADER`] value [`set_priority_handler`] stamps, so a caller
/// can tell which mutating route answered.
pub const PRIORITY_ENDPOINT: &str = "accounts-priority";

/// Path of the settings endpoint behind `tcr config`: `GET` reports the
/// rotation and limit settings this process is running ([`config_get_handler`]),
/// `POST` saves one to the config file ([`config_set_handler`]).
///
/// The `POST` exists because a running proxy owns its config file: its token
/// rotations and `tcr disable` writes are read-modify-writes under a lock in
/// this process, and a second process writing the same file outside that lock
/// can lose a change to an interleaved write. So `tcr config set` hands the
/// setting here when a proxy answers, the same posture as `tcr disable` and
/// `tcr control`.
///
/// Same shape as [`DISABLED_PATH`] and for the same reasons: registered as a
/// real route so axum matches it exactly and BEFORE the catch-all, and gated by
/// [`local_endpoint_gate`] plus, for the `POST`, the `application/json`
/// requirement.
pub const CONFIG_PATH: &str = "/_tcr/config";

/// Confirmed redemption of one banked reset for a listed account.
pub const CODEX_RESET_PATH: &str = "/_tcr/codex/reset";
/// The discriminator shared by the local route and its CLI caller.
pub const CODEX_RESET_ENDPOINT: &str = "codex-reset";

/// Confirmed claim of one banked reset for a listed Claude account
/// ([`crate::manager::Manager::reset_claude`]). Its own route rather than a wider
/// [`CODEX_RESET_PATH`], so an older proxy answers a newer CLI with a plain 404 and a
/// Codex reset keeps the route it always had.
pub const CLAUDE_RESET_PATH: &str = "/_tcr/claude/reset";
/// The discriminator shared by the Claude reset route and its CLI caller.
pub const CLAUDE_RESET_ENDPOINT: &str = "claude-reset";

/// The [`ENDPOINT_HEADER`] value both halves of [`CONFIG_PATH`] stamp.
pub const CONFIG_ENDPOINT: &str = "config";

/// The fan mode, for `tcr fan` and TcrBar (which shells out to it): `POST`
/// only, gated by [`read_command`] like [`CONFIG_PATH`]. The body is one
/// [`crate::fan::FanRequest`]; a refused value is a stamped 400 with one
/// sentence, an accepted one a stamped 202 carrying the `fanControl` object as
/// of that moment (the controller works on it after the answer). A process
/// that runs no fan controller answers a stamped 503.
///
/// Who can call it: anyone who passes [`local_endpoint_gate`], the same class
/// of caller that can already save a setting or add an account. What that
/// caller gets is a validated mode; root clamps every rpm to the fan's own
/// range, and the hot guard runs in Follow CPU (Fixed holds exactly the rpm asked).
pub const FAN_PATH: &str = "/_tcr/fan";

/// The [`ENDPOINT_HEADER`] value [`FAN_PATH`] stamps.
pub const FAN_ENDPOINT: &str = "fan";

/// Body cap for the local control route: a JSON object with three short fields.
/// Deliberately not [`MAX_BODY_BYTES`] (256 MiB, sized for model requests) — this
/// route buffers into memory in a credential-holding process, and nothing
/// legitimate here is larger than a line.
const CONTROL_BODY_LIMIT: usize = 8 * 1024;

/// The path segment [`STATUS_PATH`] lives under. Every path beneath it belongs to
/// the PROXY, never to Anthropic, so anything under it that is not a registered
/// route is answered with a LOCAL 404 (see the guard in [`handle`]) instead of
/// being forwarded. That is not hygiene: before the guard existed a typo'd status
/// probe fell through the catch-all and was sent to `api.anthropic.com/_tcr/status`
/// carrying a pooled OAuth Bearer, which burned an account on a request no
/// upstream route could ever answer.
///
/// Stored WITHOUT a trailing slash and matched by [`path_is_under`], so the bare
/// `/_tcr` is covered along with everything beneath it — but the guard is a
/// BYTE-EXACT, case-sensitive compare on the raw request target, and that is the
/// whole of what it covers. Three spellings a URL parser would fold onto this
/// prefix are NOT under it and are forwarded upstream (measured): `//_tcr/…`
/// (empty first segment), `/_TCR/…` (case) and `/%5ftcr/…` (percent-encoded `_`,
/// which `uri.path()` hands over undecoded).
///
/// That is a wart, not a hole, and it is deliberately left alone here: those
/// spellings reach Anthropic with a pooled Bearer and come back 404 — the
/// account-burn shape this guard exists to stop, at the cost of one 404 rather
/// than a mutation. They cannot mutate anything, because the local routes are
/// matched by their EXACT paths ([`STATUS_PATH`], [`DISABLED_PATH`]) before this
/// catch-all, so a spelling that misses the prefix misses the routes too. Do not
/// read this comment as "nothing unguarded gets past" — read it as "what gets past
/// is a forwarded 404".
/// `pub(crate)` so `src/peer/serve.rs` refuses THIS prefix rather than a second
/// spelling of it: a relayed request that reached these routes would arrive at
/// `local_endpoint_gate` having satisfied it by construction.
pub(crate) const LOCAL_PREFIX: &str = "/_tcr";

/// Paths whose upstream call must carry the **client's own** credential and which
/// therefore bypass account selection entirely. Matched as prefixes of the request
/// path.
///
/// Do NOT "simplify" any of these back into the catch-all. They are bound to the
/// identity that authenticated the CLIENT, so a rotated pooled token is the wrong
/// credential by construction:
/// - `/v1/code/…` is the Remote Control channel, bound to the session's paired
///   claude.ai identity; a pooled token 403s its worker event stream.
/// - `/api/oauth/files/…` and `/api/oauth/file_upload` are attachment transfers. A
///   file uploaded from claude.ai belongs to the paired identity, so fetching it
///   with a pooled token 403s and **Claude Code silently drops the image from the
///   message** — nothing surfaces the failure, the turn just loses its attachment.
/// - `/v1/mcp_servers` is the claude.ai connector list. The ids it returns are scoped
///   to the identity that asked, and the MCP traffic that follows them goes to
///   `mcp-proxy.anthropic.com` — a host the CONNECT path blind-tunnels, so it carries
///   the CLIENT's own token. A pooled list therefore hands the client ids its own
///   identity cannot resolve.
/// - `/v1/sessions` is Remote Control's session record — read, `/archive`,
///   `/unarchive`. Measured across all four rolling proxy logs
///   (the cache directory's `logs/`), aggregating `upstream response account=… path=…
///   status=…`: `/v1/sessions/<id>` and its sub-paths came back 404 on 7 of 7
///   requests, spread over 6 different pooled accounts, and never once a 2xx from
///   any pooled identity. The reason is visible in the ids: the same opaque suffix
///   appears on both planes — `/v1/code/sessions/cse_<suffix>` (relayed,
///   `mode=ClientCredential status=200`) and `/v1/sessions/session_<suffix>`
///   (pooled, 404), for the SAME `<suffix>`. Three distinct ids matched this way. The session record is minted under the client identity
///   by the `/v1/code` pairing, so reading or archiving it under a pooled token is a
///   404 by construction — the same failure shape the `/v1/code` entry documents.
///
/// Measured on the live log while every one of these still went through rotation:
/// 13 of 57 sessionless requests came back 404 (22.8%), against 0 of 1556 pinned ones.
/// The connector list was measured the same way, one `claude mcp list` per pooled
/// account: served by the wrong one, all 9 connectors reported
/// `not_found_error: "Server not found"`; served by the client's own identity, every
/// one connected. That is why this reads as intermittent — it tracks rotation.
///
/// `/api/oauth/organizations/<org uuid>/…` (marketplaces, plugin list, skill
/// download) names the CLIENT's own organization in the path, so a pooled account
/// from any other org answers 401 or 403. That was not a lost request, it was an
/// account killer: the 401 branch of [`handle`] force-refreshes the serving account
/// on every 401, each of these calls walked the whole pool, and refresh tokens are
/// single-use, so the refreshes piled up until one was rejected and the account went
/// `Error` for good. Measured on the live log on 2026-09-16, the first day Claude
/// Code made these requests. 428 of the 445 401/403 responses after a restart were on
/// this prefix, and six freshly re-logged accounts went `Error` between 7 and 30
/// minutes after the first one, while the tokens they had last refreshed were still
/// valid on disk.
///
/// Written WITHOUT trailing slashes and matched by [`path_is_under`] — an entry
/// matches the exact path or that path followed by `/`, never a longer identifier.
/// Both edges of a raw `starts_with` were live defects: `"/api/oauth/file_upload"`
/// (no terminator) also relayed `/api/oauth/file_upload_v2`, and `"/v1/code/"`
/// (with one) missed the bare `/v1/code`.
/// `pub(crate)` so `src/peer/serve.rs` reads THIS list rather than a copy of it.
/// A borrower must refuse to open a SERVE for every path here, and a copy of a
/// security list in two files is a fact in two places that will drift.
///
/// This list is no longer the only way onto the client-credential relay: a request
/// that carries its own bearer is relayed under it on EVERY non-inference path
/// (see [`relay_mode`]). The list still decides for a request with no bearer, and
/// it is still the whole of the peer borrower's refusal.
pub(crate) const CLIENT_CREDENTIAL_PREFIXES: [&str; 6] = [
    "/v1/code",
    "/api/oauth/files",
    "/api/oauth/file_upload",
    "/api/oauth/organizations",
    "/v1/mcp_servers",
    "/v1/sessions",
];

/// The CLIENT's own OAuth token refresh. Relayed raw — no auth header at all,
/// because a refresh carries its credentials in the BODY. The proxy manages its own
/// tokens via [`Manager::ensure_fresh`]; rewriting a client's refresh would inject
/// the wrong identity into an exchange that is not ours.
///
/// Matched by [`path_is_under`], like the prefixes above. It used to be an EXACT
/// compare, which let the trailing-slash spelling `/v1/oauth/token/` fall through to
/// the POOLED path — putting our Bearer on a client's token exchange, precisely what
/// the paragraph above says must never happen. Relaying a hypothetical sub-path with
/// no auth is the safe direction to be wrong in; a pooled Bearer is not.
/// `pub(crate)` for the same reason as [`CLIENT_CREDENTIAL_PREFIXES`]: it is the
/// SEVENTH path, it lives outside that array, and `src/peer/serve.rs` must
/// refuse it too.
pub(crate) const CLIENT_TOKEN_REFRESH_PATH: &str = "/v1/oauth/token";

/// `user-agent` sent by [`RelayMode::Raw`] when the client sent none: naming
/// ourselves is honest about who is calling.
const RELAY_USER_AGENT: &str = concat!("tcr/", env!("CARGO_PKG_VERSION"));

/// How a request that must NOT go through account rotation reaches upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RelayMode {
    /// Forward the client's own headers — including its `authorization` — and
    /// stream the response back. See [`CLIENT_CREDENTIAL_PREFIXES`].
    ClientCredential,
    /// Send `content-type` / `accept` / `user-agent` and nothing else, no auth in
    /// any form. See [`CLIENT_TOKEN_REFRESH_PATH`].
    Raw,
}

/// Does `path` name `base` itself, or something beneath it?
///
/// The ONE rule every path-prefix decision in this module goes through, so that a
/// route's boundary is its segment boundary and nothing else. A bare `starts_with`
/// is wrong in both directions: without a terminator `/api/oauth/file_upload` also
/// swallows `/api/oauth/file_upload_v2`, and with one `/v1/code/` misses the bare
/// `/v1/code`. Callers must pass a `base` with NO trailing slash. Pure — unit-tested.
pub(crate) fn path_is_under(path: &str, base: &str) -> bool {
    path.strip_prefix(base)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// Is `segment` a WHATWG dot segment — `.` or `..`, in any percent-encoded spelling?
///
/// The URL parser folds `%2e`/`%2E` to `.` BEFORE it classifies a segment, so `%2e%2e`
/// and `.%2e` are `..` to it. It does NOT decode `%2f`, so `..%2f` stays one opaque
/// segment and is not traversal — decoding it here would reject a legitimate path.
/// Only 1 or 2 dots are special: `...` is an ordinary name.
fn is_dot_segment(segment: &str) -> bool {
    let mut rest = segment;
    let mut dots = 0usize;
    while !rest.is_empty() {
        if let Some(tail) = rest.strip_prefix('.') {
            rest = tail;
        } else if rest.len() >= 3
            && rest.as_bytes()[0] == b'%'
            && rest.as_bytes()[1] == b'2'
            && rest.as_bytes()[2].eq_ignore_ascii_case(&b'e')
        {
            rest = &rest[3..];
        } else {
            return false;
        }
        dots += 1;
        if dots > 2 {
            return false;
        }
    }
    dots == 1 || dots == 2
}

/// Would this raw request path mean something DIFFERENT to the upstream URL parser
/// than it means to the classifiers here? If so it must never be routed at all.
///
/// The path this module makes every decision on is `uri.path()` — the raw request
/// target, verbatim. The path that goes ON THE WIRE is whatever `Url::parse` makes
/// of `upstream + path_and_query`, and that parser normalizes per WHATWG. Where the
/// two disagree, every routing decision is made about a path the upstream never sees.
/// Measured against this crate's own reqwest, `/v1/code/../../v1/messages` classified
/// as a client-credential RELAY and arrived at the upstream as `/v1/messages` — a real
/// inference bypassing `select`, the in-flight slot, the throttle, `record_served`,
/// session pinning and the whole retry ladder, carrying the CLIENT's token. `/x/../_tcr/status`
/// slipped the `/_tcr/` guard and reached the upstream WITH A POOLED BEARER on it,
/// which is the exact shape that once burned an account.
///
/// Reconciling the two representations is the fragile fix — it re-derives the
/// upstream's parser here and stays correct only while both agree forever. Instead
/// REJECT the disagreement: no legitimate Anthropic client emits a dot segment or a
/// backslash, so the ambiguous shapes cost nothing to refuse.
///
/// Two disagreements exist, both measured, not theorized:
/// - a **dot segment** (see [`is_dot_segment`]) — the parser collapses it away.
/// - a literal **backslash**, which is not a valid request-target character at all
///   (RFC 3986 `pchar`) and which WHATWG treats as a path SEPARATOR for http(s).
///   `/v1/code\foo` classified as pooled here and landed on the upstream as the
///   Remote Control path `/v1/code/foo` wearing a pooled Bearer — the under-inclusive
///   half of the same defect. `%5c` is NOT decoded by the parser, so it is left alone.
pub(crate) fn path_is_ambiguous(path: &str) -> bool {
    path.contains('\\') || path.split('/').any(is_dot_segment)
}

/// The host this request was ADDRESSED to, or `None` when nothing says.
///
/// Absolute-form / forward-proxy request lines carry an authority in the target
/// itself, which wins; otherwise it is the `Host` header minus any `:port`. `None`
/// is a real answer, not a failure: an origin-form base-URL request with no `Host`
/// (every direct-axum caller, and HTTP/1.0) names no host at all.
///
/// ONE derivation, shared by the forwarding path's misroute guard and by
/// [`local_endpoint_gate`], so "which host did the client mean" cannot come out
/// two different ways on the two paths. Pure — unit-tested.
fn target_host<'a>(uri: &'a axum::http::Uri, headers: &'a HeaderMap) -> Option<&'a str> {
    uri.host().or_else(|| {
        headers
            .get(HOST)
            .and_then(|v| v.to_str().ok())
            .map(strip_port)
    })
}

/// `authority` without its `:port`, if it has one.
///
/// A bracketed IPv6 literal is why this is not one `rsplit_once(':')`: the last
/// colon in `[::1]` is INSIDE the address, so splitting on it yields `[:` — a host
/// that matches nothing, which turned an IPv6 loopback client into a misroute. The
/// port, when present, always follows the `]`.
pub fn strip_port(authority: &str) -> &str {
    if authority.starts_with('[') {
        return authority.split_inclusive(']').next().unwrap_or(authority);
    }
    authority
        .rsplit_once(':')
        .map_or(authority, |(host, _)| host)
}

/// Is `host` a name for THIS machine's loopback interface?
///
/// The `IpAddr::is_loopback` primitive the client-peer checks use, plus the one
/// name that is loopback without being an IP literal. `[::1]` arrives bracketed in
/// a `Host` header, and the brackets are not part of the address, so they are
/// stripped before parsing — without that, the v6 loopback fails a check the v4
/// one passes. Pure — unit-tested.
/// `pub` so `tcr peer graph --serve` (`src/main.rs`, a separate crate from this
/// library) asks this question here rather than
/// keeping its own answer to it: a second spelling of "is this loopback" is a
/// second thing to get wrong, on a gate whose whole job is refusing a name that
/// resolves to loopback in somebody else's browser. Callers hand it a host with
/// no port, which is what [`strip_port`] is for.
pub fn host_is_loopback(host: &str) -> bool {
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    bare.parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback())
        || host.eq_ignore_ascii_case("localhost")
}

/// The provider whose requests each host tcr intercepts carries: every host on
/// [`crate::tls::ALLOWED_HOSTS`], and no other (`every_intercepted_host_names_its_provider`
/// pins both directions). `platform.claude.com` is Anthropic's: Claude Code refreshes its own
/// login there.
const INTERCEPTED_HOST_PROVIDERS: [(&str, Provider); 3] = [
    ("api.anthropic.com", Provider::Anthropic),
    ("platform.claude.com", Provider::Anthropic),
    (crate::provider::CODEX_HOST, Provider::Codex),
];

/// The provider an intercepted `host` (no port) belongs to, compared as
/// [`crate::tls::host_allowed`] compares it; `None` for any other host, loopback included.
fn intercepted_host_provider(host: &str) -> Option<Provider> {
    INTERCEPTED_HOST_PROVIDERS
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(host))
        .map(|&(_, provider)| provider)
}

/// The refusal `handle` answers when an intercepted `host` and the `path` (whose provider is
/// `path_provider`) name different providers. It names both, so the client's error says
/// which request was refused and why.
fn host_path_disagreement(host: &str, path: &str, path_provider: Provider) -> String {
    const fn kind(provider: Provider) -> &'static str {
        match provider {
            Provider::Anthropic => "an Anthropic",
            Provider::Codex => "a Codex",
        }
    }
    match intercepted_host_provider(host) {
        Some(host_provider) => format!(
            "Refusing {host}{path}: {host} is {} host but this is {} path (Codex paths are the \
             ones under {}/), and tcr never forwards a request, or the credential on it, to the \
             other provider.",
            kind(host_provider),
            kind(path_provider),
            crate::provider::CODEX_PATH_PREFIX,
        ),
        None => format!(
            "Refusing {host}{path}: tcr intercepts {host} but knows no provider for it, so it \
             forwards nothing there."
        ),
    }
}

/// The refusal `handle` answers when a request that came through a tunnel to `tunnel` names some
/// other host, or a `Host` it cannot read. It names the tunnel's host and the path, like
/// [`host_path_disagreement`], and does not echo the host the request named.
fn tunnel_host_disagreement(tunnel: &str, path: &str) -> String {
    format!(
        "Refusing {tunnel}{path}: the request came through a tunnel to {tunnel} but names another \
         host, and tcr serves a tunnel only for the host it was opened to."
    )
}

/// Is this `content-type` `application/json`?
///
/// The mutating local route's whole browser defence, and the reason it is a
/// content-type check rather than something cleverer: `text/plain`,
/// `application/x-www-form-urlencoded` and `multipart/form-data` are the three
/// CORS **simple** media types, so a cross-origin POST carrying one is sent with
/// NO preflight. `application/json` is not simple, and this process answers no
/// `OPTIONS` and emits no `Access-Control-*` header — so requiring it means a
/// browser's preflight fails and the POST is never sent at all.
///
/// Measured on the route as merged (#71), from a loopback peer with no proxy
/// api-key configured, which is a fresh install: `text/plain` and
/// `application/x-www-form-urlencoded` both returned **200 and parked a live
/// account**. That the page cannot read the reply is irrelevant — the entire
/// payoff of a mutating route IS the side effect.
///
/// ABSENT is a refusal too: a request with no `content-type` is simple as well.
/// Only the media type is compared, case-insensitively — RFC 9110 makes the type
/// case-insensitive and a `; charset=utf-8` parameter legitimate, and a check that
/// rejected either would break real callers while closing nothing.
fn is_json_content_type(headers: &HeaderMap) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|value| {
            value.split(';').next().is_some_and(|media_type| {
                media_type.trim().eq_ignore_ascii_case("application/json")
            })
        })
}

/// Does this request carry positive evidence that a BROWSER initiated it on
/// behalf of some other site?
///
/// Two independent signals, either of which is disqualifying:
/// - an `Origin` header. A cross-origin POST always carries one, and this process
///   serves no HTML, so there is no page whose same-origin `Origin` we would need
///   to allow — any value at all means a browser context we do not own.
/// - `Sec-Fetch-Site` anything other than `same-origin` or `none` — the browser
///   stating the relationship itself. `none` is a user-initiated request (typed
///   URL, bookmark), not a site's.
///
/// A non-browser caller sends neither, so this costs `tcr disable` nothing. It is
/// deliberately additive to [`is_json_content_type`] rather than a replacement:
/// the content-type check is what closes the no-preflight class, and this is what
/// keeps a future local route that is NOT JSON from reopening it. Neither closes
/// DNS rebinding, where the page is genuinely same-origin — that is
/// [`host_is_loopback`]'s job.
fn is_cross_site_request(headers: &HeaderMap) -> bool {
    if headers.contains_key(axum::http::header::ORIGIN) {
        return true;
    }
    headers
        .get("sec-fetch-site")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|site| !matches!(site.trim(), "same-origin" | "none"))
}

/// Classify a request as one of the rotation-bypassing relays, or `None` for the
/// normal pooled-credential forwarding path. Takes the path WITHOUT its query, so
/// a `?…` can never smuggle a path past the match. Pure — unit-tested.
///
/// Sound only on a path [`path_is_ambiguous`] has already rejected: on a path that
/// still contains a dot segment, what this classifies is not what goes on the wire.
///
/// `client_bearer` is whether the request arrived with an `Authorization: Bearer`
/// of its own. With one, every path that is not inference is relayed under it:
/// the bookkeeping calls (`/api/claude_cli/bootstrap`, `/api/oauth/profile`,
/// `/api/organizations/<org>/…`, feature gates, telemetry) all answer "who am I
/// and what am I allowed", and a pooled account answers that for the wrong
/// person. The prefix list above stays for two reasons this rule does not cover:
/// a client with no bearer at all, and the peer borrower's refusal
/// (`src/peer/serve.rs`), which is path-only.
///
/// A Codex path (`crate::provider::Provider::of_path`) other than the one
/// inference path is relayed with or without a bearer: Codex has no bookkeeping
/// call a pooled account may answer, so nothing under `/backend-api/` but
/// Codex model calls reach the rotation; its other paths retain the client credential.
fn relay_mode(method: &Method, path: &str, client_bearer: bool) -> Option<RelayMode> {
    if *method == Method::POST && path_is_under(path, CLIENT_TOKEN_REFRESH_PATH) {
        return Some(RelayMode::Raw);
    }
    let provider = Provider::of_path(path);
    if CLIENT_CREDENTIAL_PREFIXES
        .iter()
        .any(|base| path_is_under(path, base))
        // Codex's model calls spend pooled quota. Its other paths (usage, threads,
        // plugins) retain the client's identity.
        || match provider {
            Provider::Anthropic => false,
            Provider::Codex => !provider.is_inference_path(path),
        }
    {
        return Some(RelayMode::ClientCredential);
    }
    if client_bearer && !is_inference_path(path) {
        return Some(RelayMode::ClientCredential);
    }
    None
}

/// The paths that spend quota and therefore ALWAYS take the pooled path,
/// whatever credential the client sent: Anthropic's `/v1/messages` and
/// `/v1/messages/count_tokens`, and Codex's `/backend-api/codex/responses`.
/// Exact match, and one list: [`crate::provider::Provider::is_inference_path`],
/// which `manager::select::classify_request` reads too.
///
/// One exception, decided by `handle` rather than here: a Codex request that names a
/// workspace routing of its own ([`names_codex_routing`]) is relayed on its own
/// credential, inference included, and never pooled.
fn is_inference_path(path: &str) -> bool {
    Provider::of_path(path).is_inference_path(path)
}

/// The client's own OAuth bearer, if the request carries one. `Bearer` is
/// matched case-insensitively (RFC 9110 §11.1); an empty token is no bearer.
fn client_bearer_of(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.trim().split_once(char::is_whitespace)?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then_some(token)
}

/// The `controlIdentity` check — see [`crate::control_identity`]. `Some` is the
/// refusal to return; `None` means the request proceeds.
///
/// Runs on every ANTHROPIC request that carries a client bearer, relayed or
/// pooled: a session on the wrong account must fail at its FIRST request, which
/// is a bootstrap call, not at the first turn. Inert without a control account,
/// in mode `off`, for a request with no bearer, and for every Codex request: the
/// check resolves the bearer through Anthropic's `/api/oauth/profile`, so asking
/// it about a ChatGPT token would hand that token to Anthropic (the call site in
/// `handle` says so). The operator-facing consequence: under
/// `controlIdentity: refuse`, Codex clients are never identity-checked; whatever
/// ChatGPT login a Codex client runs under, its inference is served from the
/// pool unless it names a workspace routing of its own ([`names_codex_routing`]),
/// and its other paths, and such a routed request, are relayed under its own
/// bearer. Fail-open on `Unknown` — a profile fetch that failed is logged at
/// debug and the request is served.
async fn refuse_foreign_client(manager: &Manager, bearer: &str) -> Option<Response> {
    use crate::config::ControlIdentityMode;

    let mode = manager.control_identity();
    if mode == ControlIdentityMode::Off {
        return None;
    }
    let foreign = manager.foreign_client(bearer).await?;
    let (control_email, client_shown) = (foreign.control_email, foreign.client_shown);
    if foreign.first_warning {
        tracing::warn!(
            client = %client_shown,
            control_account = %control_email,
            mode = mode.as_str(),
            "control identity: the client is signed in as a different account than controlAccount"
        );
    }
    if mode == ControlIdentityMode::Warn {
        return None;
    }
    // Anthropic requests only: see the call site in `handle_request`.
    Some(
        ClientError::new(
            StatusCode::FORBIDDEN,
            ErrorKind::Permission,
            format!(
                "tcr: this client is signed in as {client_shown}, but controlAccount is {control_email}, \
                 so its connectors, plugins and settings would come from the wrong account. \
                 Run /logout, then /login as {control_email} — or set \"controlIdentity\": \"warn\" \
                 (or \"off\") in the tcr config to serve it anyway."
            ),
        )
        .respond(Provider::Anthropic),
    )
}

/// Build the proxy router. Every method and path funnels through the single
/// catch-all [`handle`] — EXCEPT [`STATUS_PATH`], which the proxy answers itself.
/// The [`Manager`] is shared state.
pub fn app(manager: Arc<Manager>) -> Router {
    Router::new()
        // Registered as a real route so it is matched BEFORE the catch-all: a
        // status request must never reach `handle`, which would rewrite it to
        // Anthropic with a pooled OAuth Bearer attached.
        //
        // `.fallback` on the METHOD router pins the wrong-method answer to a local
        // 405. Measured on axum 0.8: a bare `get(...)` ALREADY answers 405 rather
        // than inheriting the Router's catch-all, so this is belt-and-braces, not
        // a fix for a live bug — it buys a consistent JSON error body and, more to
        // the point, pins the behaviour explicitly so a future axum whose method
        // routers inherit the outer fallback cannot silently turn a POST on this
        // path into an upstream-forwarded request carrying a pooled OAuth Bearer.
        .route(
            STATUS_PATH,
            axum::routing::get(status_handler).fallback(status_method_not_allowed),
        )
        // The quota read: a pure GET like the status route, registered the same
        // way and for the same reasons.
        .route(
            QUOTA_PATH,
            axum::routing::get(quota_handler).fallback(quota_method_not_allowed),
        )
        // The routing document: a pure GET like the status route, registered the same way.
        .route(
            ROUTING_PATH,
            axum::routing::get(routing_handler)
                .head(routing_method_not_allowed)
                .fallback(routing_method_not_allowed),
        )
        // The two MUTATING local routes. Same reasoning as above, and the method
        // fallback matters more here: an unregistered method must answer a local
        // 405, never fall through to `handle` and be forwarded upstream.
        .route(
            DISABLED_PATH,
            axum::routing::post(set_disabled_handler)
                .fallback(|| async { method_not_allowed(&DISABLED_ROUTE) }),
        )
        .route(
            ADD_ACCOUNT_PATH,
            axum::routing::post(add_account_handler)
                .fallback(|| async { method_not_allowed(&ADD_ACCOUNT_ROUTE) }),
        )
        .route(
            CONTROL_PATH,
            axum::routing::post(set_control_handler)
                .fallback(|| async { method_not_allowed(&CONTROL_ROUTE) }),
        )
        .route(
            PRIORITY_PATH,
            axum::routing::post(set_priority_handler)
                .fallback(|| async { method_not_allowed(&PRIORITY_ROUTE) }),
        )
        .route(
            CONFIG_PATH,
            axum::routing::get(config_get_handler)
                .post(config_set_handler)
                .fallback(|| async { method_not_allowed(&CONFIG_ROUTE) }),
        )
        .route(
            CODEX_RESET_PATH,
            axum::routing::post(codex_reset_handler)
                .fallback(|| async { reset_no_store(method_not_allowed(&CODEX_RESET_ROUTE)) }),
        )
        .route(
            CLAUDE_RESET_PATH,
            axum::routing::post(claude_reset_handler)
                .fallback(|| async { reset_no_store(method_not_allowed(&CLAUDE_RESET_ROUTE)) }),
        )
        .route(
            FAN_PATH,
            axum::routing::post(fan_handler).fallback(|| async { method_not_allowed(&FAN_ROUTE) }),
        )
        .route(
            PEER_DIAL_PATH,
            axum::routing::post(peer_dial_handler)
                .fallback(|| async { method_not_allowed(&PEER_DIAL_ROUTE) }),
        )
        .fallback(handle)
        .with_state(manager)
}

/// The two gates every locally-answered `/_tcr/…` route passes, or the refusal to
/// return. `Some(response)` means REFUSED; `None` means proceed.
///
/// ONE implementation, deliberately: these routes live on a process holding every
/// account's OAuth access and refresh token, and the gate is the whole of their
/// authorization. Two copies of it drift, and the copy that drifts is the one on
/// whichever route was added later — which is also the more dangerous route,
/// since the second one added mutates rotation.
///
/// 1. **Origin.** The peer must be loopback, proven by the [`ClientAddr`]
///    extension the hybrid listener injects from the real socket address (the
///    same extension the auth gate in [`handle`] uses). It is not a header, so a
///    client cannot forge it. Absent — a request that did not arrive through the
///    listener — we fail CLOSED. Bind scope is not authorization: `127.0.0.1` is
///    reachable by every process and every container on this host, so "we only
///    bind loopback" is not a claim about who is calling.
/// 2. **Key.** When a proxy api-key is configured it is REQUIRED here, with no
///    loopback exemption — deliberately stricter than [`handle`], which exempts
///    loopback because `claude` authenticates with its own OAuth and never sends
///    the proxy key. Nothing on this host needs to read or steer the fleet without
///    the operator's secret, so doing either costs the same secret that using the
///    proxy does. The compare is [`key_matches`] (constant-time, length-safe).
/// 3. **Addressed to us.** The host the request names ([`target_host`]) must be
///    loopback, or absent. This is the DNS-rebinding check, and it is the only one
///    of the four that closes it: a page served from a name resolving to 127.0.0.1
///    is genuinely SAME-ORIGIN with this process, so it sends no `Origin`, needs no
///    preflight, and may use any content type — the one thing that still gives it
///    away is the name it addressed us by. It also refuses an absolute-form
///    (forward-proxy) request line, which is never how a caller reaches a route
///    that only exists here.
/// 4. **Not cross-site.** [`is_cross_site_request`] — belt-and-braces for a
///    browser that did preflight, or a future local route that is not JSON.
///
/// Checks 3 and 4 were added after the FIRST mutating route shipped under this
/// gate: a read that a page cannot read is harmless, but the account-control route
/// is the first one whose entire payoff is the side effect, and a browser on this
/// host is a loopback caller. They live here, on the shared gate, rather than on
/// the mutating handler — see the paragraph above about which copy drifts. What
/// stays route-local is the `application/json` requirement
/// ([`is_json_content_type`]), because a GET has no body to type.
///
/// `endpoint` names the route in the 403 body only. It is not a capability: no
/// value of it weakens any check.
fn local_endpoint_gate(
    parts: &axum::http::request::Parts,
    manager: &Manager,
    endpoint: &str,
) -> Option<Response> {
    let client_is_loopback = parts
        .extensions
        .get::<ClientAddr>()
        .is_some_and(|a| a.0.ip().is_loopback());
    if !client_is_loopback {
        return Some(
            ClientError::new(
                StatusCode::FORBIDDEN,
                ErrorKind::Permission,
                format!("The tcr {endpoint} endpoint is loopback-only."),
            )
            .respond_local(),
        );
    }

    if let Some(expected) = manager.proxy_api_key() {
        let provided = parts.headers.get("x-api-key").and_then(|v| v.to_str().ok());
        if !key_matches(provided, expected) {
            return Some(
                ClientError::new(
                    StatusCode::UNAUTHORIZED,
                    ErrorKind::Authentication,
                    "Missing or invalid x-api-key.",
                )
                .respond_local(),
            );
        }
    }

    if let Some(host) = target_host(&parts.uri, &parts.headers)
        && !host_is_loopback(host)
    {
        tracing::debug!(
            target_host = %host,
            endpoint,
            "refused a tcr endpoint request addressed to a non-loopback host"
        );
        return Some(
            ClientError::new(
                StatusCode::MISDIRECTED_REQUEST,
                ErrorKind::InvalidRequest,
                format!(
                    "The tcr {endpoint} endpoint answers only to a loopback host, \
                     not to '{host}'."
                ),
            )
            .respond_local(),
        );
    }

    if is_cross_site_request(&parts.headers) {
        return Some(
            ClientError::new(
                StatusCode::FORBIDDEN,
                ErrorKind::Permission,
                format!("The tcr {endpoint} endpoint does not serve cross-site requests."),
            )
            .respond_local(),
        );
    }

    None
}

/// A non-`GET` on [`STATUS_PATH`]. Answered locally with 405 so the request is
/// neither forwarded upstream nor able to mutate anything — the endpoint is a
/// pure read, and there is no verb on it that is not.
async fn status_method_not_allowed() -> Response {
    ClientError::new(
        StatusCode::METHOD_NOT_ALLOWED,
        ErrorKind::InvalidRequest,
        "The tcr status endpoint is read-only: GET.",
    )
    .respond_local()
}

/// `GET /_tcr/status` — the live fleet snapshot, for `tcr status`.
///
/// This is an **attack surface on a process holding every account's OAuth access
/// and refresh token**, so it is gated twice — origin and key, see
/// [`local_endpoint_gate`] for both and for why bind scope is not authorization —
/// and reads nothing but state that is already on screen in the TUI, plus the
/// machine's heat.
///
/// It takes only [`Parts`](axum::http::request::Parts), so the request body is
/// never read. It touches no account state, triggers no account probe, no token
/// refresh and no config write. The one thing it does besides read is the heat:
/// [`crate::heat::current`] may run an AppleSMC read on the blocking pool and
/// writes the result into that module's own cache, waiting at most
/// [`crate::heat::READ_DEADLINE`] for it. It returns the [`crate::status`]
/// projection, which carries no token, no key and no `Authorization` echo (see
/// that module's no-secret invariant; a temperature and fan speeds are not
/// secrets).
async fn status_handler(
    State(manager): State<Arc<Manager>>,
    parts: axum::http::request::Parts,
) -> Response {
    if let Some(refusal) = local_endpoint_gate(&parts, &manager, "status") {
        return refusal;
    }

    let now = OffsetDateTime::now_utc();
    let payload = crate::status::StatusPayload::from_snapshot(
        &manager.snapshot(now),
        &manager.thresholds(),
        manager.http1_only(),
        manager.control_name(),
        manager.group_colors(),
    );
    // The peers block, which `from_snapshot` leaves empty because a
    // `StatsSnapshot` is the accounts view and carries no peers file, no peer
    // state file and no ledger. Assigned HERE, in the serving process, which is
    // the only caller that holds the path those files were installed under
    // (`peer::tunnel::egress::peers_path`), the CLI's own round-trip tests and
    // `into_snapshot` keep the honest empty list.
    //
    // Two file reads on a status request, and that is the trade this endpoint
    // already makes everywhere else: it is a snapshot of what this process
    // believes right now, and a cached peers block would be the stale-panel bug
    // the whole payload exists to end. Both reads are read-only and neither
    // takes the file lock.
    //
    // Re-examined and kept. The cost was raised again because the panel polls
    // this endpoint every three seconds, which is two reads of two small files
    // out of the page cache per poll; what a cache would buy is smaller than
    // the failure it would reintroduce, and this repository has already lost an
    // evening to a panel rendering a config the server had not re-read. A
    // reader who wants this cheaper should make the panel poll less often
    // rather than make the answer older than the question.
    let payload = payload.with_serving_facts(crate::status::ServingFacts {
        peers: crate::peer::status::rows::peers_block_with_error(
            &crate::peer::tunnel::egress::peers_path(),
            (now.unix_timestamp_nanos() / 1_000_000) as i64,
        ),
        mitm: manager.mitm_status(),
        codex_traffic: manager.codex_traffic((now.unix_timestamp_nanos() / 1_000_000) as i64),
        codex_upstream: manager.upstream_for(Provider::Codex),
        request_key: manager.request_key_mode().map(|mode| {
            let (connects, requests) = manager.unkeyed_wire();
            (mode, connects, requests)
        }),
        // Read on the blocking pool and kept for two seconds (`crate::heat`), so a
        // burst of status requests costs one SMC read and none of it runs on this
        // worker. Absent where there is no AppleSMC.
        heat: crate::heat::current().await,
        // A copy of the controller's last published state: no SMC read, no lock
        // held past the copy.
        fan_control: manager.fan().map(|fan| fan.row()),
    });
    let Ok(body) = serde_json::to_string(&payload) else {
        // Serializing plain numbers and strings cannot realistically fail, but a
        // proxy never panics on a client request — surface it as a 500 instead.
        return ClientError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorKind::Api,
            "Could not serialize the status snapshot.",
        )
        .respond_local();
    };
    let mut response = Response::new(Body::from(body));
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    // Operational state that is stale the instant it is read, from a
    // credential-holding process — never store it anywhere.
    headers.insert("cache-control", HeaderValue::from_static("no-store"));
    response
}

/// A non-`GET` on [`QUOTA_PATH`]. A local 405, for the reasons
/// [`status_method_not_allowed`] gives.
async fn quota_method_not_allowed() -> Response {
    ClientError::new(
        StatusCode::METHOD_NOT_ALLOWED,
        ErrorKind::InvalidRequest,
        "The tcr quota endpoint is read-only: GET.",
    )
    .respond_local()
}

/// `GET /_tcr/quota`: per account its gate and its 5-hour and weekly
/// utilization with resets, the [`crate::status::QuotaPayload`] projection.
///
/// Gated exactly as [`status_handler`] is, through [`local_endpoint_gate`]. It
/// answers from the manager's in-memory snapshot: no probe, no token refresh,
/// no upstream call and, unlike the status route, no peers-file read, so a
/// status line can call it on every turn. The one disk access is the config
/// `stat` inside [`Manager::snapshot`], which re-reads the file only when its
/// mtime moved.
async fn quota_handler(
    State(manager): State<Arc<Manager>>,
    parts: axum::http::request::Parts,
) -> Response {
    if let Some(refusal) = local_endpoint_gate(&parts, &manager, "quota") {
        return refusal;
    }

    let payload =
        crate::status::QuotaPayload::from_snapshot(&manager.snapshot(OffsetDateTime::now_utc()));
    let Ok(body) = serde_json::to_string(&payload) else {
        return ClientError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorKind::Api,
            "Could not serialize the quota snapshot.",
        )
        .respond_local();
    };
    let mut response = Response::new(Body::from(body));
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert("cache-control", HeaderValue::from_static("no-store"));
    response
}

/// A non-`GET` on [`ROUTING_PATH`]. A local 405, for the reasons
/// [`status_method_not_allowed`] gives.
async fn routing_method_not_allowed() -> Response {
    ClientError::new(
        StatusCode::METHOD_NOT_ALLOWED,
        ErrorKind::InvalidRequest,
        "The tcr routing endpoint is read-only: GET.",
    )
    .respond_local()
}

/// `GET /_tcr/routing`: the routing document, [`crate::routing`]'s schema 1.
///
/// Gated exactly as [`status_handler`] is, through [`local_endpoint_gate`]. It answers from
/// memory: the selector's own candidate evaluation, read without stamping, pinning or
/// spending anything ([`Manager::routing_candidates`]), and what this boot observed. No
/// probe, no token refresh, no upstream call. It names no account, organization, session or
/// credential.
async fn routing_handler(
    State(manager): State<Arc<Manager>>,
    parts: axum::http::request::Parts,
) -> Response {
    if let Some(refusal) = local_endpoint_gate(&parts, &manager, "routing") {
        return refusal;
    }

    let Some(body) = crate::routing::document(&manager, OffsetDateTime::now_utc())
        .and_then(|document| serde_json::to_string(&document).ok())
    else {
        return ClientError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorKind::Api,
            "Could not build the routing document.",
        )
        .respond_local();
    };
    let mut response = Response::new(Body::from(body));
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert("cache-control", HeaderValue::from_static("no-store"));
    response
}

/// One local `/_tcr/*` route that takes commands: the name its gate and its
/// messages use, the [`ENDPOINT_HEADER`] value it stamps, and the verbs it
/// takes. One const per route, so the name a person reads and the stamp the CLI
/// reads come from one place and cannot drift apart.
struct LocalRoute {
    /// The route in the words of its messages: "The tcr {name} endpoint ...".
    name: &'static str,
    /// The [`ENDPOINT_HEADER`] value, a wire contract the CLI matches.
    stamp: &'static str,
    /// The verbs the 405 names: `POST`, or `GET or POST`.
    verbs: &'static str,
    /// Whether every stamped answer also carries [`PROVIDERS_HEADER`].
    advertises_providers: bool,
}

const CODEX_RESET_ROUTE: LocalRoute = LocalRoute {
    name: "codex reset",
    stamp: CODEX_RESET_ENDPOINT,
    verbs: "POST",
    advertises_providers: false,
};

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CodexResetRequest {
    query: String,
    idempotency_key: String,
}

fn reset_no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert("cache-control", HeaderValue::from_static("no-store"));
    response
}

async fn codex_reset_handler(State(manager): State<Arc<Manager>>, req: Request) -> Response {
    let response = match read_command::<CodexResetRequest>(
        &CODEX_RESET_ROUTE,
        &manager,
        req,
        "Expected query and a valid idempotencyKey, and no other fields.",
    )
    .await
    {
        Err(response) => *response,
        Ok(parsed)
            if parsed.query.is_empty()
                || !crate::manager::valid_reset_key(&parsed.idempotency_key) =>
        {
            route_error(
                &CODEX_RESET_ROUTE,
                StatusCode::BAD_REQUEST,
                ErrorKind::InvalidRequest,
                "Expected query and a valid idempotencyKey, and no other fields.",
            )
        }
        Ok(parsed) => match manager
            .reset_codex(&parsed.query, &parsed.idempotency_key)
            .await
        {
            Ok(result) => json_ok(&CODEX_RESET_ROUTE, &result),
            Err(error) => {
                use crate::manager::CodexResetError;
                let status = match error {
                    CodexResetError::Missing => StatusCode::NOT_FOUND,
                    CodexResetError::Ambiguous | CodexResetError::Unavailable => {
                        StatusCode::CONFLICT
                    }
                    CodexResetError::WrongProvider => StatusCode::BAD_REQUEST,
                    CodexResetError::Failed => StatusCode::BAD_GATEWAY,
                    CodexResetError::Unknown => StatusCode::GATEWAY_TIMEOUT,
                };
                route_error(
                    &CODEX_RESET_ROUTE,
                    status,
                    ErrorKind::InvalidRequest,
                    error.message(),
                )
            }
        },
    };
    reset_no_store(response)
}

const CLAUDE_RESET_ROUTE: LocalRoute = LocalRoute {
    name: "claude reset",
    stamp: CLAUDE_RESET_ENDPOINT,
    verbs: "POST",
    advertises_providers: false,
};

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ClaudeResetRequest {
    query: String,
    idempotency_key: String,
    /// `weekly`, `full`, `session` or `fable`; absent when the account holds one kind.
    #[serde(default)]
    kind: Option<String>,
}

async fn claude_reset_handler(State(manager): State<Arc<Manager>>, req: Request) -> Response {
    const INVALID: &str =
        "Expected query, a valid idempotencyKey and an optional kind, and no other fields.";
    let response =
        match read_command::<ClaudeResetRequest>(&CLAUDE_RESET_ROUTE, &manager, req, INVALID).await
        {
            Err(response) => *response,
            Ok(parsed) => {
                let kind = parsed
                    .kind
                    .as_deref()
                    .map(|token| tcr_status_wire::ResetKind::parse(token).ok_or(()));
                match kind {
                    Some(Err(())) => route_error(
                        &CLAUDE_RESET_ROUTE,
                        StatusCode::BAD_REQUEST,
                        ErrorKind::InvalidRequest,
                        INVALID,
                    ),
                    _ if parsed.query.is_empty()
                        || !crate::claude_resets::valid_request_id(&parsed.idempotency_key) =>
                    {
                        route_error(
                            &CLAUDE_RESET_ROUTE,
                            StatusCode::BAD_REQUEST,
                            ErrorKind::InvalidRequest,
                            INVALID,
                        )
                    }
                    kind => match manager
                        .reset_claude(
                            &parsed.query,
                            kind.and_then(Result::ok),
                            &parsed.idempotency_key,
                        )
                        .await
                    {
                        Ok(result) => json_ok(&CLAUDE_RESET_ROUTE, &result),
                        Err(error) => {
                            use crate::manager::ClaudeResetError;
                            let status = match error {
                                ClaudeResetError::Missing => StatusCode::NOT_FOUND,
                                ClaudeResetError::Ambiguous | ClaudeResetError::Unavailable => {
                                    StatusCode::CONFLICT
                                }
                                ClaudeResetError::WrongProvider
                                | ClaudeResetError::NotRead
                                | ClaudeResetError::NoneHeld
                                | ClaudeResetError::KindNeeded => StatusCode::BAD_REQUEST,
                                ClaudeResetError::Auth => StatusCode::FORBIDDEN,
                                ClaudeResetError::Failed => StatusCode::BAD_GATEWAY,
                                ClaudeResetError::Unknown => StatusCode::GATEWAY_TIMEOUT,
                            };
                            route_error(
                                &CLAUDE_RESET_ROUTE,
                                status,
                                ErrorKind::InvalidRequest,
                                error.message(),
                            )
                        }
                    },
                }
            }
        };
    reset_no_store(response)
}

const DISABLED_ROUTE: LocalRoute = LocalRoute {
    name: "disable-account",
    stamp: DISABLED_ENDPOINT,
    verbs: "POST",
    advertises_providers: false,
};

const ADD_ACCOUNT_ROUTE: LocalRoute = LocalRoute {
    name: "account-add",
    stamp: ADD_ACCOUNT_ENDPOINT,
    verbs: "POST",
    advertises_providers: true,
};

const CONTROL_ROUTE: LocalRoute = LocalRoute {
    name: "control-account",
    stamp: CONTROL_ENDPOINT,
    verbs: "POST",
    advertises_providers: false,
};

const PRIORITY_ROUTE: LocalRoute = LocalRoute {
    name: "priority",
    stamp: PRIORITY_ENDPOINT,
    verbs: "POST",
    advertises_providers: false,
};

const CONFIG_ROUTE: LocalRoute = LocalRoute {
    name: "settings",
    stamp: CONFIG_ENDPOINT,
    verbs: "GET or POST",
    advertises_providers: false,
};

const FAN_ROUTE: LocalRoute = LocalRoute {
    name: "fan",
    stamp: FAN_ENDPOINT,
    verbs: "POST",
    advertises_providers: false,
};

const PEER_DIAL_ROUTE: LocalRoute = LocalRoute {
    name: "peer dial",
    stamp: PEER_DIAL_ENDPOINT,
    verbs: "POST",
    advertises_providers: false,
};

/// `POST /_tcr/peer/dial`: dial a tailnet address through tcr's own tailnet
/// node and hand the connection back to the local caller as a byte stream.
///
/// # Why this exists
///
/// The knock, the first pairing and the join run in the CLI process, and the
/// TcrBar Trust button spawns that CLI. tcr's own node lives in THIS process,
/// and a Mac in node mode has no `utun` for the kernel to route a `100.x`
/// address through. So the CLI asks here; its Noise code is unchanged, and
/// only where its socket comes from differs. See `crate::peer::tailnet_node`.
///
/// # What guards it
///
/// [`local_endpoint_gate`], like every `/_tcr/` route: loopback caller, the
/// api key when one is set, a loopback host, not cross-site. Then it dials
/// only a tailnet address, and only through the node: it is never a general
/// connect-anywhere relay, and it does nothing in a process running no node.
/// The dial happens BEFORE the `101`, so a refusal is an ordinary answer the
/// CLI can read, not a stream that closes.
async fn peer_dial_handler(State(manager): State<Arc<Manager>>, request: Request) -> Response {
    let (mut parts, _body) = request.into_parts();
    if let Some(refusal) = local_endpoint_gate(&parts, &manager, PEER_DIAL_ROUTE.name) {
        return refusal;
    }
    let refuse = |status: StatusCode, message: &str| {
        route_error(&PEER_DIAL_ROUTE, status, ErrorKind::InvalidRequest, message)
    };
    let wants_upgrade = parts
        .headers
        .get(axum::http::header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case(PEER_DIAL_UPGRADE));
    if !wants_upgrade {
        return refuse(
            StatusCode::BAD_REQUEST,
            &format!("The tcr peer dial endpoint answers `Upgrade: {PEER_DIAL_UPGRADE}` only."),
        );
    }
    let Some(target) = parts
        .headers
        .get(PEER_DIAL_TARGET_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<SocketAddr>().ok())
    else {
        return refuse(
            StatusCode::BAD_REQUEST,
            &format!(
                "The tcr peer dial endpoint needs a socket address in `{PEER_DIAL_TARGET_HEADER}`."
            ),
        );
    };
    let Some(on_upgrade) = parts.extensions.remove::<hyper::upgrade::OnUpgrade>() else {
        return refuse(
            StatusCode::BAD_REQUEST,
            "The tcr peer dial endpoint needs an HTTP/1.1 connection it can upgrade.",
        );
    };
    use crate::peer::tailnet_node::RouteDial;
    let stream = match crate::peer::tailnet_node::dial_for_route(
        target,
        crate::peer::tailnet_node::DIAL_TIMEOUT,
    )
    .await
    {
        RouteDial::NoNode | RouteDial::NotTailnet => {
            return route_error(
                &PEER_DIAL_ROUTE,
                StatusCode::CONFLICT,
                ErrorKind::InvalidRequest,
                &format!(
                    "This tcr is not running its own tailnet node, or {target} is not a tailnet \
                     address."
                ),
            );
        }
        // Gil, 2026-10-20: peer ports only, refused before the node is asked
        // for anything (`tailnet_node::allowed_dial_ports`).
        RouteDial::PortRefused(why) => {
            return route_error(
                &PEER_DIAL_ROUTE,
                StatusCode::FORBIDDEN,
                ErrorKind::Permission,
                &format!("The tcr peer dial endpoint refused: {why}."),
            );
        }
        RouteDial::Failed(err) => {
            return route_error(
                &PEER_DIAL_ROUTE,
                StatusCode::BAD_GATEWAY,
                ErrorKind::Api,
                &format!("Could not reach {target} through the tailnet node: {err:#}"),
            );
        }
        RouteDial::Dialled(stream) => stream,
    };

    tokio::spawn(async move {
        match on_upgrade.await {
            Ok(upgraded) => {
                let (up, down) = crate::peer::tailnet_node::splice_until_either_ends(
                    hyper_util::rt::TokioIo::new(upgraded),
                    stream,
                )
                .await;
                tracing::debug!(
                    target = %target,
                    bytes_up = up,
                    bytes_down = down,
                    "peer dial: a CLI dial through the tailnet node ended"
                );
            }
            Err(err) => tracing::debug!(
                target = %target,
                error = %err,
                "peer dial: the CLI's connection was not upgraded"
            ),
        }
    });

    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
    let headers = response.headers_mut();
    headers.insert(
        axum::http::header::CONNECTION,
        HeaderValue::from_static("upgrade"),
    );
    headers.insert(
        axum::http::header::UPGRADE,
        HeaderValue::from_static(PEER_DIAL_UPGRADE),
    );
    stamp(&PEER_DIAL_ROUTE, response)
}

/// Add [`ENDPOINT_HEADER`] to a response, naming which local route produced it
/// (and [`PROVIDERS_HEADER`] beside it where the route advertises them): the one
/// place they are set, so no stamped answer can carry one without the other.
fn stamp(route: &LocalRoute, mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(ENDPOINT_HEADER, HeaderValue::from_static(route.stamp));
    if route.advertises_providers
        && let Ok(providers) = HeaderValue::from_str(&advertised_providers())
    {
        response.headers_mut().insert(PROVIDERS_HEADER, providers);
    }
    response
}

/// A stamped error answer from `route`.
fn route_error(route: &LocalRoute, status: StatusCode, kind: ErrorKind, message: &str) -> Response {
    stamp(
        route,
        ClientError::new(status, kind, message).respond_local(),
    )
}

/// A stamped JSON 200 from `route`, marked `no-store`.
fn json_ok<T: serde::Serialize>(route: &LocalRoute, payload: &T) -> Response {
    let Ok(body) = serde_json::to_string(payload) else {
        return route_error(
            route,
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorKind::Api,
            &format!("Could not serialize the {} result.", route.name),
        );
    };
    let mut response = Response::new(Body::from(body));
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert("cache-control", HeaderValue::from_static("no-store"));
    stamp(route, response)
}

/// A wrong verb on `route`: a local 405, never forwarded upstream and never able
/// to mutate anything. Stamped, so a caller can tell this 405 (the route exists,
/// wrong verb) from the local 404 a tcr without the route returns.
fn method_not_allowed(route: &LocalRoute) -> Response {
    route_error(
        route,
        StatusCode::METHOD_NOT_ALLOWED,
        ErrorKind::InvalidRequest,
        &format!("The tcr {} endpoint takes {}.", route.name, route.verbs),
    )
}

/// Run the checks every mutating `/_tcr/*` route shares, in this order, and
/// return the parsed command body or the answer to send instead.
///
/// 1. [`local_endpoint_gate`]. Its refusal is NOT stamped: a refusal must not
///    confirm the route exists to a caller that failed the gate, and the CLI
///    never needs the discriminator on a 401/403, both terminal for it.
/// 2. `application/json`. It is not a CORS simple media type, so requiring it
///    forces a preflight this process never answers; see
///    [`is_json_content_type`] for the four shapes that reached a handler and
///    parked a live account before the check existed.
/// 3. The body, read to [`CONTROL_BODY_LIMIT`].
/// 4. A JSON OBJECT of type `T` ([`parse_object`]); `shape` is the 400 text.
///
/// From step 2 on the answer is stamped, because it is a request-shape error and
/// the CLI reads the stamp to tell a route that answered from a tcr too old to
/// have one.
async fn read_command<T: serde::de::DeserializeOwned>(
    route: &LocalRoute,
    manager: &Manager,
    req: Request,
    shape: &str,
) -> Result<T, Box<Response>> {
    let (parts, body) = req.into_parts();
    if let Some(refusal) = local_endpoint_gate(&parts, manager, route.name) {
        return Err(Box::new(refusal));
    }
    if !is_json_content_type(&parts.headers) {
        return Err(Box::new(route_error(
            route,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            ErrorKind::InvalidRequest,
            &format!(
                "The tcr {} endpoint requires Content-Type: application/json.",
                route.name
            ),
        )));
    }
    let Ok(bytes) = to_bytes(body, CONTROL_BODY_LIMIT).await else {
        return Err(Box::new(route_error(
            route,
            StatusCode::BAD_REQUEST,
            ErrorKind::InvalidRequest,
            "Could not read the request body.",
        )));
    };
    parse_object::<T>(&bytes).ok_or_else(|| {
        Box::new(route_error(
            route,
            StatusCode::BAD_REQUEST,
            ErrorKind::InvalidRequest,
            shape,
        ))
    })
}

/// Deserialize `bytes` into `T`, but ONLY from a JSON object — `None` for
/// anything else, including a JSON ARRAY.
///
/// The array case is why this exists rather than a bare
/// `serde_json::from_slice`. Serde will happily build a struct out of a
/// sequence, positionally, when the element count matches the field count. So
/// `["alice@example.com", true]` deserializes into a two-field
/// [`SetDisabledRequest`] and parks an account — a body nobody meant as a
/// request, accepted because of how many fields the struct happens to have
/// today. Removing one field is all it took to open that: this route had three
/// fields and the same array was a 400.
///
/// Requiring an object is the property the callers actually mean, and it does
/// not move when a field is added or removed.
fn parse_object<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Option<T> {
    let value: Value = serde_json::from_slice(bytes).ok()?;
    if !value.is_object() {
        return None;
    }
    serde_json::from_value(value).ok()
}

/// The request body of [`DISABLED_PATH`].
///
/// `query` and `disabled` are `Option` rather than required fields so a body that
/// omits either is a 400 naming the field, instead of axum's own rejection text:
/// this route is what a person reaches for when the fleet is already misbehaving,
/// and an unhelpful 400 there is its own outage.
#[derive(serde::Deserialize)]
struct SetDisabledRequest {
    query: Option<String>,
    disabled: Option<bool>,
}

/// The 200 body of [`DISABLED_PATH`]. Deserializable too — `tcr enable`/`disable`
/// read it, and both halves of the wire contract belong to one type.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct SetDisabledResponse {
    /// The RESOLVED account name, so the answer names what was actually parked.
    ///
    /// It is NOT a partial or fuzzy match: [`crate::identity::match_accounts`] is
    /// the account's exact name, byte-for-byte, case-sensitive and untrimmed.
    /// Measured against a fleet holding `alice@example.com`: `alice`, `alice@`,
    /// `example`, `ALICE@EXAMPLE.COM` and a trailing space are each a 404.
    pub name: String,
    /// The state now in force in the live rotation.
    pub disabled: bool,
    /// Whether the config file also carries it. `false` with a `warning` is a
    /// change that is live but will not survive a restart.
    pub persisted: bool,
    /// [`crate::manager::DisablePersist::warning`], verbatim, or `null`.
    pub warning: Option<String>,
}

/// `POST /_tcr/accounts/disabled` — park or unpark an account IN THE LIVE
/// ROTATION, for `tcr disable` / `tcr enable`.
///
/// The endpoint exists because the flag was previously only ever read at boot:
/// see [`DISABLED_PATH`] for the defect. This is the only route on the proxy that
/// changes what the next request is routed to, so it is gated exactly as
/// [`status_handler`] is — [`local_endpoint_gate`], no weaker for being a write —
/// and it does the work through [`Manager::set_disabled_by_query`], which is the
/// same call the TUI's `d` key makes. It does not invent a resolution rule: the
/// CLI's own [`crate::identity::match_one`] runs against the server's live
/// rotation slots, so the two cannot disagree about which account a query names.
///
/// The durable half is NOT swallowed. `persisted: false` plus a `warning` is a
/// change that is in force right now and will vanish on restart, which is
/// precisely the state the old code left the operator in silently.
async fn set_disabled_handler(State(manager): State<Arc<Manager>>, req: Request) -> Response {
    let parsed: SetDisabledRequest = match read_command(
        &DISABLED_ROUTE,
        &manager,
        req,
        "Expected a JSON object: {\"query\": \"<account>\", \"disabled\": true}.",
    )
    .await
    {
        Ok(parsed) => parsed,
        Err(refusal) => return *refusal,
    };
    let (Some(query), Some(disabled)) = (parsed.query, parsed.disabled) else {
        return route_error(
            &DISABLED_ROUTE,
            StatusCode::BAD_REQUEST,
            ErrorKind::InvalidRequest,
            "Both \"query\" (string) and \"disabled\" (bool) are required.",
        );
    };
    if query.trim().is_empty() {
        return route_error(
            &DISABLED_ROUTE,
            StatusCode::BAD_REQUEST,
            ErrorKind::InvalidRequest,
            "\"query\" must not be empty.",
        );
    }

    match manager.set_disabled_by_query(&query, disabled) {
        SetDisabledOutcome::NoMatch => route_error(
            &DISABLED_ROUTE,
            StatusCode::NOT_FOUND,
            ErrorKind::NotFound,
            &format!("No account in the live rotation matches '{query}'."),
        ),
        SetDisabledOutcome::Ambiguous(names) => route_error(
            &DISABLED_ROUTE,
            StatusCode::CONFLICT,
            ErrorKind::InvalidRequest,
            &config::ambiguous_query_message(&query, &names),
        ),
        SetDisabledOutcome::Applied { name, persist } => json_ok(
            &DISABLED_ROUTE,
            &SetDisabledResponse {
                name,
                disabled,
                persisted: matches!(persist, DisablePersist::Persisted),
                warning: persist.warning(disabled).map(str::to_string),
            },
        ),
    }
}

/// The request body of [`CONTROL_PATH`].
///
/// `query: null` (or the field omitted) CLEARS the control account —
/// deliberately not an error, unlike [`SetDisabledRequest`] where a missing
/// `query` is a 400: there is no destructive-vs-informative ambiguity here,
/// clearing is a complete and meaningful request on its own.
#[derive(serde::Deserialize, Default)]
struct SetControlRequest {
    #[serde(default)]
    query: Option<String>,
}

/// The 200 body of [`CONTROL_PATH`]. Deserializable too — `tcr control` reads
/// it, and both halves of the wire contract belong to one type.
///
/// Every field `#[serde(default)]`, with the safer reading as the default —
/// same cross-build posture [`SetDisabledResponse`] would want if a future
/// tcr shipped a field this one predates: an OLDER client parsing a NEWER
/// server's body (or vice versa) must land on "nothing happened / not saved"
/// rather than silently assume success on a field it never received.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct SetControlResponse {
    /// The RESOLVED account name now set as control, or `None` when this call
    /// cleared it. Resolved rather than echoed for the same reason
    /// [`SetDisabledResponse::name`] is.
    #[serde(default)]
    pub name: Option<String>,
    /// Whether this call cleared the control account (`query` was `null`).
    #[serde(default)]
    pub cleared: bool,
    /// Whether the config file also carries it. `false` with a `warning` is a
    /// change that is live but will not survive a restart.
    #[serde(default)]
    pub persisted: bool,
    /// [`crate::manager::ControlPersist::warning`], verbatim, or `null`.
    #[serde(default)]
    pub warning: Option<String>,
}

/// The 200 body of [`PRIORITY_PATH`]. Deserializable too, because `tcr
/// priority` reads it and both halves of the wire contract belong to one type.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct SetPriorityResponse {
    /// The RESOLVED account name, for the same reason as
    /// [`SetDisabledResponse::name`].
    pub name: String,
    /// The priority now in force in the live rotation.
    pub priority: i64,
}

/// The request body of [`PRIORITY_PATH`]. Both fields are `Option` so a body
/// missing either is a 400 naming the field, the same reasoning as
/// [`SetDisabledRequest`].
#[derive(serde::Deserialize)]
struct SetPriorityRequest {
    query: Option<String>,
    priority: Option<i64>,
}

/// `POST /_tcr/accounts/priority`: set one account's priority IN THE LIVE
/// ROTATION, for `tcr priority`.
///
/// Gated by [`read_command`]. The
/// work is [`Manager::set_priority_by_query`], which changes memory only; the
/// CLI writes the config file, because `--first`/`--last` are computed from it.
async fn set_priority_handler(State(manager): State<Arc<Manager>>, req: Request) -> Response {
    let parsed: SetPriorityRequest = match read_command(
        &PRIORITY_ROUTE,
        &manager,
        req,
        "Expected a JSON object: {\"query\": \"<account>\", \"priority\": 0}.",
    )
    .await
    {
        Ok(parsed) => parsed,
        Err(refusal) => return *refusal,
    };
    let (Some(query), Some(priority)) = (parsed.query, parsed.priority) else {
        return route_error(
            &PRIORITY_ROUTE,
            StatusCode::BAD_REQUEST,
            ErrorKind::InvalidRequest,
            "Both \"query\" (string) and \"priority\" (integer) are required.",
        );
    };
    if query.trim().is_empty() {
        return route_error(
            &PRIORITY_ROUTE,
            StatusCode::BAD_REQUEST,
            ErrorKind::InvalidRequest,
            "\"query\" must not be empty.",
        );
    }

    match manager.set_priority_by_query(&query, priority) {
        SetPriorityOutcome::NoMatch => route_error(
            &PRIORITY_ROUTE,
            StatusCode::NOT_FOUND,
            ErrorKind::NotFound,
            &format!("No account in the live rotation matches '{query}'."),
        ),
        SetPriorityOutcome::Ambiguous(names) => route_error(
            &PRIORITY_ROUTE,
            StatusCode::CONFLICT,
            ErrorKind::InvalidRequest,
            &config::ambiguous_query_message(&query, &names),
        ),
        SetPriorityOutcome::Applied { name } => {
            json_ok(&PRIORITY_ROUTE, &SetPriorityResponse { name, priority })
        }
    }
}

/// The request body of a `POST` to [`CONFIG_PATH`]: the key as the file spells
/// it and the value in the words `tcr config set` takes
/// ([`crate::config_set::Setting::parse`]).
#[derive(serde::Deserialize)]
struct SetConfigRequest {
    key: String,
    value: String,
}

/// The 200 body of a `POST` to [`CONFIG_PATH`]. Deserializable too, because
/// `tcr config set` reads it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct SetConfigResponse {
    /// The key as the file spells it.
    pub key: String,
    /// The value saved, in [`crate::config_set::Setting::value_text`]'s words.
    pub value: String,
    /// Whether the file changed; `false` when it already said this.
    pub changed: bool,
}

/// `GET /_tcr/config`: the settings this process is running
/// ([`Manager::running_settings`]), the `running` half of `tcr config show`.
/// A read, gated like [`quota_handler`]; the refusal is not stamped, so a
/// caller that failed the gate learns nothing about the route.
async fn config_get_handler(
    State(manager): State<Arc<Manager>>,
    parts: axum::http::request::Parts,
) -> Response {
    if let Some(refusal) = local_endpoint_gate(&parts, &manager, CONFIG_ROUTE.name) {
        return refusal;
    }
    json_ok(&CONFIG_ROUTE, &manager.running_settings())
}

/// `POST /_tcr/config`: save one setting to the config file, for `tcr config
/// set`, gated by [`read_command`]. The value is parsed
/// here again by the same [`crate::config_set::Setting::parse`] the CLI ran,
/// so this route refuses what the CLI refuses, in the same words, whoever
/// calls it. The work is [`Manager::persist_setting`], which writes the file
/// and leaves what this process runs alone.
///
/// Every refusal is a stamped 400 or 409, which `tcr config set` reads as
/// "the proxy said no" and writes nothing.
async fn config_set_handler(State(manager): State<Arc<Manager>>, req: Request) -> Response {
    let SetConfigRequest { key, value } = match read_command(
        &CONFIG_ROUTE,
        &manager,
        req,
        "Expected a JSON object: {\"key\": \"switchThreshold\", \"value\": \"0.9\"}.",
    )
    .await
    {
        Ok(parsed) => parsed,
        Err(refusal) => return *refusal,
    };
    let setting = match crate::config_set::Setting::parse(&key, &value) {
        Ok(setting) => setting,
        Err(refusal) => {
            return route_error(
                &CONFIG_ROUTE,
                StatusCode::BAD_REQUEST,
                ErrorKind::InvalidRequest,
                &refusal.to_string(),
            );
        }
    };
    match manager.persist_setting(&setting) {
        Ok(write) => json_ok(
            &CONFIG_ROUTE,
            &SetConfigResponse {
                key: setting.key().as_str().to_string(),
                value: setting.value_text(),
                changed: write == crate::config_set::SettingWrite::Updated,
            },
        ),
        Err(crate::manager::SettingPersistError::NoConfigFile) => route_error(
            &CONFIG_ROUTE,
            StatusCode::CONFLICT,
            ErrorKind::InvalidRequest,
            "This proxy was started without a config file, so it has nowhere to save a setting. Nothing was changed.",
        ),
        Err(crate::manager::SettingPersistError::WriteFailed(why)) => route_error(
            &CONFIG_ROUTE,
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorKind::Api,
            &format!("The proxy could not write its config file: {why}"),
        ),
    }
}

/// `POST /_tcr/fan`: hand one fan mode to the controller ([`FAN_PATH`]).
///
/// Never waits on the fans: the controller may be inside a 15 s unlock, and
/// the mode is queued behind it (latest wins). The 202 body is the
/// `fanControl` object as it stood when the command went in, with `command`
/// the number the command was sent under ([`crate::fan::FanHandle::send`]);
/// `tcr fan` polls the status route until `fanControl.command` reaches it and
/// the state settles. A 1.2.12 `tcr fan` reads the rest, as it always did.
async fn fan_handler(State(manager): State<Arc<Manager>>, req: Request) -> Response {
    let request: crate::fan::FanRequest = match read_command(
        &FAN_ROUTE,
        &manager,
        req,
        "Expected a JSON object: {\"mode\": \"auto\"} (optionally with \"force\": true), \
         {\"mode\": \"fixed\", \"rpm\": 5000} or \
         {\"mode\": \"followCpu\", \"quietBelowC\": 60, \"fullAtC\": 90}.",
    )
    .await
    {
        Ok(parsed) => parsed,
        Err(refusal) => return *refusal,
    };
    let command = match request.command() {
        Ok(command) => command,
        Err(refusal) => {
            return route_error(
                &FAN_ROUTE,
                StatusCode::BAD_REQUEST,
                ErrorKind::InvalidRequest,
                &format!("The fan mode was refused: {refusal}"),
            );
        }
    };
    let Some(fan) = manager.fan() else {
        return route_error(
            &FAN_ROUTE,
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorKind::Api,
            "This tcr server runs no fan controller.",
        );
    };
    match fan.send(command) {
        Ok(accepted) => {
            let mut response = json_ok(&FAN_ROUTE, &accepted);
            if response.status() == StatusCode::OK {
                *response.status_mut() = StatusCode::ACCEPTED;
            }
            response
        }
        Err(crate::fan::NotRunning) => route_error(
            &FAN_ROUTE,
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorKind::Api,
            "The fan controller has stopped; restart tcr to control fans.",
        ),
    }
}

/// `POST /_tcr/accounts/control` — set or clear the identity-bound CONTROL
/// account IN THE LIVE ROTATION, for `tcr control`.
///
/// Gated by [`read_command`]. Does the work through
/// [`Manager::set_control_by_query`], the same call `tcr control` makes.
///
/// Unlike [`set_disabled_handler`], a missing/`null` `query` is not a 400 — it
/// is the CLEAR request, a complete operation on its own.
async fn set_control_handler(State(manager): State<Arc<Manager>>, req: Request) -> Response {
    let parsed: SetControlRequest = match read_command(
        &CONTROL_ROUTE,
        &manager,
        req,
        "Expected a JSON object: {\"query\": \"<account>\" | null}.",
    )
    .await
    {
        Ok(parsed) => parsed,
        Err(refusal) => return *refusal,
    };
    if matches!(&parsed.query, Some(q) if q.trim().is_empty()) {
        return route_error(
            &CONTROL_ROUTE,
            StatusCode::BAD_REQUEST,
            ErrorKind::InvalidRequest,
            "\"query\" must not be empty; omit it or send null to clear.",
        );
    }
    let cleared = parsed.query.is_none();

    match manager.set_control_by_query(parsed.query.as_deref()) {
        SetControlOutcome::NoMatch => route_error(
            &CONTROL_ROUTE,
            StatusCode::NOT_FOUND,
            ErrorKind::NotFound,
            &format!(
                "No account in the live rotation matches '{}'.",
                parsed.query.unwrap_or_default()
            ),
        ),
        SetControlOutcome::Ambiguous(names) => route_error(
            &CONTROL_ROUTE,
            StatusCode::CONFLICT,
            ErrorKind::InvalidRequest,
            &config::ambiguous_query_message(&parsed.query.unwrap_or_default(), &names),
        ),
        // 409, like the tie above: every `tcr control` client reads a stamped 404 or 409
        // from this route as a refusal of the query and writes nothing, where any other
        // status would send it to the file instead.
        SetControlOutcome::CodexAccount(name) => route_error(
            &CONTROL_ROUTE,
            StatusCode::CONFLICT,
            ErrorKind::InvalidRequest,
            &crate::manager::codex_control_refusal(&name),
        ),
        SetControlOutcome::Applied { name, persist } => json_ok(
            &CONTROL_ROUTE,
            &SetControlResponse {
                name,
                cleared,
                persisted: matches!(persist, ControlPersist::Persisted),
                warning: persist.warning().map(str::to_string),
            },
        ),
    }
}

/// The request body of [`ADD_ACCOUNT_PATH`].
///
/// Mirrors [`config::Account`]'s wire shape (camelCase, `type` for
/// `account_type`) so a caller can build the body directly from the credentials
/// a login already produced. `name` and `access_token` are `Option` — like
/// [`SetDisabledRequest`]'s fields — so a body missing either gets a specific
/// 400 naming the field instead of axum's own rejection text.
///
/// `priority` and `switch_threshold` are accepted for the NEW-account case only;
/// see [`add_account_handler`]. `disabled` is deliberately not accepted at all —
/// an account reaching this route is one the operator wants serving traffic now.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddAccountRequest {
    name: Option<String>,
    #[serde(rename = "type", default)]
    account_type: Option<config::AccountType>,
    #[serde(default)]
    account_uuid: Option<String>,
    #[serde(default)]
    org_uuid: Option<String>,
    #[serde(default)]
    org_name: Option<String>,
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_at: Option<i64>,
    #[serde(default)]
    priority: Option<i64>,
    #[serde(default)]
    switch_threshold: Option<f64>,
    /// The account's plan, as the login that produced these credentials read it
    /// off the profile endpoint (see [`config::Account::organization_type`]).
    ///
    /// Named explicitly rather than inherited: this struct MIRRORS
    /// `config::Account`'s wire shape, it is not that type, so a field added to
    /// `Account` alone is silently dropped here — the body still deserializes,
    /// the route still answers 200, and the account lands with no plan while
    /// the file route has one. `add_account_request_carries_the_plan_fields`
    /// pins it.
    #[serde(default)]
    organization_type: Option<String>,
    #[serde(default)]
    rate_limit_tier: Option<String>,
    #[serde(default)]
    seat_tier: Option<String>,
    #[serde(default)]
    plan_display_name: Option<String>,
    /// The upstream the credential belongs to. Absent is Anthropic: every body
    /// posted before this field existed, and every Anthropic row, which
    /// `config::Account` serializes with no `provider` key. A Codex import
    /// (`login::login_from_codex`) posts `"provider": "codex"`. Without this
    /// field the route dropped it, filed that credential as an Anthropic account
    /// and resolved it onto any Anthropic row sharing its name or id, overwriting
    /// that row's own single-use refresh token.
    /// `add_account_request_carries_the_provider` pins it.
    #[serde(default)]
    provider: Provider,
    /// The folder a Codex login was imported from ([`config::Account::codex_home`]), named for
    /// the same seam reason as the plan fields above: without it the route drops the key, and a
    /// live import records no folder while the file route does.
    /// `add_account_request_carries_the_codex_home` pins it.
    #[serde(default)]
    codex_home: Option<String>,
}

/// The 200 body of [`ADD_ACCOUNT_PATH`]. Deserializable too — unit 3's CLI
/// deserializes it, and both halves of a wire contract belong to one type.
///
/// This is a cross-BUILD wire contract — the same class as
/// [`crate::singleton::ProxyHost::Unknown`] — because the `tcr` that
/// deserializes a reply can be older than the `tcr` that served it. Every
/// field below carries `#[serde(default)]` so a field a NEWER server has
/// renamed or dropped does not fail the whole deserialize: `live_client::post_add_account`
/// maps a deserialize failure on a 2xx to `LiveControlError::Unusable`, and
/// `login::probe_add_route` reads that as `AddCapability::Unusable` — a
/// *successful* live add would then look indistinguishable from a wedged or
/// route-less proxy, and `login::admit_login` REFUSES outright (bridge item D)
/// instead of silently falling back to the file beside a server that just
/// handled the request correctly. Field ADDITIONS are already safe on their
/// own (an older CLI just ignores a key it doesn't know); this attribute is
/// what makes removals and renames safe too. Each default is chosen as the
/// SAFER of the two readings, not merely the type's zero value: `persisted`
/// defaults to `false` (never claim durability we can't confirm) and `added`
/// defaults to `false` (never claim a fresh account was created when it
/// might have been an in-place credential replacement) — same
/// degrade-to-the-safer-state shape as
/// `ProxyHost::Unknown`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct AddAccountResponse {
    /// The account's name AS STORED. For an update this is the EXISTING row's
    /// name, which may differ from what was submitted (e.g. a bare email
    /// meeting a stored `"email (org)"` display name) — never a name that only
    /// ever existed in the request.
    #[serde(default)]
    pub name: String,
    /// `true` when this identity had no match in the live rotation and was
    /// appended; `false` when an existing account's credentials were replaced
    /// in place. "Created" and "credentials replaced" are different facts and
    /// the operator needs to know which one they got.
    #[serde(default)]
    pub added: bool,
    /// The account's index in the live rotation. Stable for the process
    /// lifetime — append-only, never reused, and an update never moves it.
    #[serde(default)]
    pub index: usize,
    /// Whether the config file also carries it. `false` with a `warning` is a
    /// change that is live but will not survive a restart.
    #[serde(default)]
    pub persisted: bool,
    /// [`AddPersist::warning`] and/or the no-refresh-token warning, joined when
    /// both apply, or `null`.
    #[serde(default)]
    pub warning: Option<String>,
    /// The provider of the row the server STORED, read back from the live
    /// rotation rather than echoed from the request, so a client can confirm a
    /// Codex login was stored as one. `None` means the server predates this
    /// field — and a server that predates the field may predate `provider`
    /// itself, which stores every account as a Claude account (see
    /// [`PROVIDERS_HEADER`]).
    #[serde(default)]
    pub provider: Option<Provider>,
}

/// Shown when the submitted account carries no refresh token: it will serve
/// until its access token expires and then go dead, silently, unless the
/// operator hears about it now. `pub(crate)` so [`crate::login::login_with_token`]
/// can print the same hazard on the offline (file) route too — that path
/// never goes through this module's `add_account_handler`, so nothing else
/// would say it.
pub(crate) const NO_REFRESH_TOKEN_WARNING: &str = "this account has no refresh token — it will serve until its access token expires, then go dead";

/// Join whichever of the durable-persist warning and the no-refresh-token
/// warning actually apply into the single `warning` field the response carries.
fn combine_warnings(persist: Option<&str>, no_refresh_token: Option<&str>) -> Option<String> {
    match (persist, no_refresh_token) {
        (None, None) => None,
        (Some(a), None) => Some(a.to_string()),
        (None, Some(b)) => Some(b.to_string()),
        (Some(a), Some(b)) => Some(format!("{a} {b}")),
    }
}

/// Build the 200 body of [`ADD_ACCOUNT_PATH`], stamped like every response this
/// route produces.
fn add_account_response(
    name: String,
    added: bool,
    index: usize,
    persist: AddPersist,
    warning: Option<String>,
    provider: Option<Provider>,
) -> Response {
    json_ok(
        &ADD_ACCOUNT_ROUTE,
        &AddAccountResponse {
            name,
            added,
            index,
            persisted: matches!(persist, AddPersist::Persisted),
            warning,
            provider,
        },
    )
}

/// `POST /_tcr/accounts` — add an account to the LIVE rotation with no proxy
/// restart, for a `tcr login` that should not have to stop and restart the
/// process to take effect.
///
/// Gated by [`read_command`] like every other mutating route — because this
/// route carries an OAuth access token and refresh token in its body and is a
/// write, so it is authorized no more weakly than the read routes.
///
/// The behaviour is [`Manager::add_or_update_account`]'s: the submitted identity
/// resolves against the live rotation via [`crate::identity::match_one`], and
/// either appends (nothing matched — a brand new account) or replaces an
/// existing account's credentials in place (exactly one matched — a re-login).
/// See that function's doc-comment for why the two cases use different
/// resolution anchors for their durable write. An ambiguous match is a 409
/// naming the candidates, via [`crate::config::ambiguous_query_message`] — never
/// guessed.
async fn add_account_handler(State(manager): State<Arc<Manager>>, req: Request) -> Response {
    let parsed: AddAccountRequest = match read_command(
        &ADD_ACCOUNT_ROUTE,
        &manager,
        req,
        "Expected a JSON object: {\"name\": \"<account>\", \"accessToken\": \"<token>\", …}.",
    )
    .await
    {
        Ok(parsed) => parsed,
        Err(refusal) => return *refusal,
    };
    // LOAD-BEARING CAPABILITY SIGNAL: `login::probe_add_route` deliberately
    // POSTs a blank name to trigger exactly this branch, and reads a STAMPED
    // 400 back as proof this route exists. Changing the status (e.g. to 422),
    // or answering without the route's stamp, silently turns that read into
    // `AddCapability::Unusable` — `login::admit_login` then REFUSES outright (bridge
    // item D) rather than falling back to the file, so this degrades to a
    // needless refusal beside a server that actually has the route, not the
    // whole-file clobber this feature exists to remove. See the seam test
    // `probe_add_capabilitys_blank_body_is_a_stamped_400_and_mutates_nothing`.
    let Some(name) = parsed.name.filter(|n| !n.trim().is_empty()) else {
        return route_error(
            &ADD_ACCOUNT_ROUTE,
            StatusCode::BAD_REQUEST,
            ErrorKind::InvalidRequest,
            "\"name\" (string) is required and must not be empty.",
        );
    };
    // Same load-bearing signal as the blank-name branch above — the probe's
    // body is ALSO blank on `accessToken`, so whichever of the two checks
    // fires first is the one the probe actually exercises. Keep this one a
    // stamped 400 too.
    let Some(access_token) = parsed.access_token.filter(|t| !t.trim().is_empty()) else {
        return route_error(
            &ADD_ACCOUNT_ROUTE,
            StatusCode::BAD_REQUEST,
            ErrorKind::InvalidRequest,
            "\"accessToken\" (string) is required and must not be empty.",
        );
    };
    // The same rule `config::load` applies to a file row: tcr does not serve
    // API-key accounts. Without this, one added here would serve, and only
    // the next load would set it aside.
    let account_type = parsed.account_type.unwrap_or_default();
    if matches!(account_type, config::AccountType::ApiKey) {
        return route_error(
            &ADD_ACCOUNT_ROUTE,
            StatusCode::BAD_REQUEST,
            ErrorKind::InvalidRequest,
            "tcr does not serve API-key accounts; add a Claude login instead.",
        );
    }
    let refresh_token = parsed.refresh_token.filter(|t| !t.trim().is_empty());
    let no_refresh_token_warning = refresh_token.is_none().then_some(NO_REFRESH_TOKEN_WARNING);

    let account = config::Account {
        name,
        account_type,
        provider: parsed.provider,
        account_uuid: parsed.account_uuid,
        org_uuid: parsed.org_uuid,
        org_name: parsed.org_name,
        access_token,
        refresh_token,
        expires_at: parsed.expires_at,
        priority: parsed.priority,
        switch_threshold: parsed.switch_threshold,
        disabled: None,
        groups: None,
        organization_type: parsed.organization_type,
        rate_limit_tier: parsed.rate_limit_tier,
        seat_tier: parsed.seat_tier,
        plan_display_name: parsed.plan_display_name,
        egress: config::Egress::Local,
        egress_strict: false,
        codex_home: parsed.codex_home,
        extra: serde_json::Map::new(),
    };
    // Captured before the move below: on an ambiguous match this is the ONLY
    // copy of what the caller actually searched for.
    let query_name = account.name.clone();

    match manager.add_or_update_account(account) {
        AddAccountOutcome::Ambiguous(names) => route_error(
            &ADD_ACCOUNT_ROUTE,
            StatusCode::CONFLICT,
            ErrorKind::InvalidRequest,
            &config::ambiguous_query_message(&query_name, &names),
        ),
        // The provider comes off the stored row, not the request: the echo is the
        // server's account of what it holds, which is what a client checks.
        AddAccountOutcome::Added { idx, name, persist } => {
            let warning = combine_warnings(persist.warning(), no_refresh_token_warning);
            add_account_response(name, true, idx, persist, warning, manager.provider(idx))
        }
        AddAccountOutcome::Updated { idx, name, persist } => {
            let warning = combine_warnings(persist.warning(), no_refresh_token_warning);
            add_account_response(name, false, idx, persist, warning, manager.provider(idx))
        }
    }
}

/// Whether a dropped SSE tee chunk represents genuine evidence loss for the
/// truncation classifier. `evidence_dropped` asserts exactly one thing: that
/// bytes arrived from UPSTREAM which the parser never got to see. `Full`
/// answers yes — the bounded channel could not keep up, those bytes are
/// gone, and "no `message_stop`" is no longer trustworthy. `Closed` answers
/// a different question: the CONSUMER (the parser task's own `rx`-backed
/// stream) went away. A consumer going away is never evidence that upstream
/// data was lost, so it must not latch the same flag — the distinction holds
/// no matter why `rx` closed, including a parser that later gains a read
/// timeout, an explicit cancellation, or has its task aborted at shutdown.
///
/// `eventsource-stream`'s `Utf8Stream` never surfaces a UTF-8 error
/// mid-stream — an unresolved tail is buffered, not rejected, until the
/// underlying byte stream itself reaches EOF — so `parse_sse_usage`'s
/// malformed-frame `break` cannot close `rx` while the upstream is still
/// sending; that specific race is not constructible through real TCP
/// behavior. What IS reachable: a parser-side panic drops `rx` while the
/// upstream keeps streaming, and without this split every subsequent
/// `try_send` on that request would latch `evidence_dropped`, making the
/// classifier silently abstain on a turn whose evidence it never actually
/// lost — an unrelated bug quietly disabling this feature.
const fn is_genuine_evidence_loss<T>(err: &tokio::sync::mpsc::error::TrySendError<T>) -> bool {
    matches!(err, tokio::sync::mpsc::error::TrySendError::Full(_))
}

/// Whether a Codex 403 actually rejects the pooled login. Codex only attempts authentication
/// recovery for a 401 (`model-provider/src/provider.rs`); its 403s also include Cloudflare and
/// policy blocks (`codex-api/src/api_bridge.rs`). Require the explicit JSON authentication
/// signals `CLIProxyAPI` recognizes (`codex_executor_terminal.go`), and let a challenge header
/// override even an authentication-shaped body. Unknown errors are passed through unchanged.
fn codex_forbidden_is_auth_error(headers: &HeaderMap, body: &[u8]) -> bool {
    if headers
        .get("cf-mitigated")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("challenge"))
    {
        return false;
    }
    let Ok(parsed) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    let Some(error) = parsed.get("error") else {
        return false;
    };
    error
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind.trim().eq_ignore_ascii_case("authentication_error"))
        || error
            .get("code")
            .and_then(Value::as_str)
            .is_some_and(|code| {
                ["invalid_api_key", "unauthorized"]
                    .iter()
                    .any(|known| code.trim().eq_ignore_ascii_case(known))
            })
}

/// Explicit account/workspace access failures can be avoided by trying another login.
/// Unknown JSON, HTML, region and Cloudflare blocks are request/network failures: rotating
/// or benching credentials cannot fix them. Keep their original response for the client.
fn codex_forbidden_is_account_error(headers: &HeaderMap, body: &[u8]) -> bool {
    if headers
        .get("cf-mitigated")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("challenge"))
    {
        return false;
    }
    let Ok(document) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    let Some(error) = document.get("error") else {
        return false;
    };

    [error.get("code"), error.get("type")]
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .any(|code| {
            [
                "workspace_deactivated",
                "deactivated_workspace",
                "workspace_disabled",
                "account_deactivated",
                "account_disabled",
                "organization_deactivated",
            ]
            .iter()
            .any(|known| code.trim().eq_ignore_ascii_case(known))
        })
}

/// Whole milliseconds from `from` to `to`, zero if `to` is not later. Saturates at
/// `u64::MAX` rather than wrapping: a duration that long is not a request.
fn ms_between(from: Instant, to: Instant) -> u64 {
    u64::try_from(to.saturating_duration_since(from).as_millis()).unwrap_or(u64::MAX)
}

/// One pooled request's clock. Its fields ride on the request's `request usage`
/// line when it has one; otherwise they go on a DEBUG `request timing` line, and
/// a request tcr itself held for [`SLOW_WAIT_MS`] also gets an INFO `slow request`
/// line (see [`TimingFields`]).
///
/// The request path used to log no time at all, so "is tcr slow, and where" had no
/// answer from the logs. Every field is an `Instant` or a counter, set at the point
/// it names, and nothing here is touched per stream chunk except one `Option` check
/// on the first chunk and one store at the end.
///
/// - `wait_ms`: arrival until the FINAL attempt's `send()` was called. Everything
///   tcr did first: reading the body, choosing an account, waiting out holds,
///   earlier failed attempts and the final attempt's own throttle wait.
/// - `throttle_ms`: total time inside `Manager::throttle_send`, all attempts. Not
///   the pinned-egress path's own pacing, which happens inside `peer::tunnel::egress`.
/// - `upstream_ms`: the final attempt's `send()` until its response headers.
/// - `first_byte_ms`: arrival until the first body chunk was handed to the client
///   side of the tee (streamed), or until the whole body was read (buffered).
/// - `total_ms`: arrival until the stream ended, or until the response was built.
///
/// A field with no value (no send was ever made, say) is left out of the line.
#[derive(Debug)]
struct RequestTiming {
    arrived: Instant,
    /// False until the request is known to take the pooled path. A request
    /// rejected or relayed before then logs no `request timing` line, and a
    /// streamed one hands its clock to the body and disarms this copy.
    armed: bool,
    attempts: u32,
    throttle: Duration,
    send_started: Option<Instant>,
    headers_at: Option<Instant>,
    first_account: Option<usize>,
    final_account: Option<usize>,
    account: Option<String>,
    model: Option<String>,
    session: Option<u64>,
    stream: bool,
    first_byte_at: Option<Instant>,
    /// When the last attempt's buffered body finished reading.
    buffered_at: Option<Instant>,
    ended_at: Option<Instant>,
}

impl RequestTiming {
    const fn starting_at(arrived: Instant) -> Self {
        Self {
            arrived,
            armed: false,
            attempts: 0,
            throttle: Duration::ZERO,
            send_started: None,
            headers_at: None,
            first_account: None,
            final_account: None,
            account: None,
            model: None,
            session: None,
            stream: false,
            first_byte_at: None,
            buffered_at: None,
            ended_at: None,
        }
    }

    /// An upstream `send()` is about to be called for account `idx`. Each call
    /// resets the header mark, so `upstream_ms` is always the LAST attempt's.
    fn note_send(&mut self, idx: usize, account: Option<&str>) {
        self.attempts += 1;
        self.first_account.get_or_insert(idx);
        self.final_account = Some(idx);
        self.account = account.map(str::to_owned);
        self.headers_at = None;
        self.send_started = Some(Instant::now());
    }

    /// The request is on the pooled path: from here on, every response it ends in
    /// logs this clock.
    const fn arm(&mut self) {
        self.armed = true;
    }

    /// The request's model and session, known once its body is read, ride on its line.
    fn describe(&mut self, model: Option<&str>, session: Option<u64>) {
        self.model = model.map(str::to_owned);
        self.session = session;
    }

    /// One wait in `Manager::throttle_send`, begun at `started`, ends now.
    fn note_throttle(&mut self, started: Instant) {
        self.throttle += started.elapsed();
    }

    /// The attempt's response headers arrived.
    fn note_headers(&mut self) {
        self.headers_at = Some(Instant::now());
    }

    /// A streamed body chunk is handed on toward the client; only the first one counts.
    fn note_first_byte(&mut self) {
        if self.first_byte_at.is_none() {
            self.first_byte_at = Some(Instant::now());
        }
    }

    /// The attempt's buffered body is read whole, now.
    fn mark_body_buffered(&mut self) {
        self.buffered_at = Some(Instant::now());
    }

    /// The buffered body is handed on toward the client: its read instant is the first byte.
    const fn forward_buffered_body(&mut self) {
        if let Some(at) = self.buffered_at {
            self.first_byte_at = Some(at);
        }
    }

    /// The upstream ended the stream (EOF or a transport error).
    fn mark_ended(&mut self) {
        self.ended_at = Some(Instant::now());
    }

    /// Finishes a streamed request's clock: at the stream's end, `completed`, or now
    /// when the stream never ended (the client walked away).
    fn finish_stream(&mut self, status: u16) -> TimingFields {
        let completed = self.ended_at.is_some();
        let end = self.ended_at.unwrap_or_else(Instant::now);
        self.finish(status, end, completed)
    }

    /// Moves the clock out as a streamed request's, leaving a disarmed copy behind,
    /// so the streamed body owns the one line this request logs.
    const fn hand_off_stream(&mut self) -> Self {
        let arrived = self.arrived;
        let mut owned = std::mem::replace(self, Self::starting_at(arrived));
        owned.stream = true;
        owned
    }

    /// Finishes the clock for the request's `request usage` line, and disarms it so
    /// [`handle`] does not log it a second time.
    fn finish_for_usage_line(&mut self, status: u16, end: Instant) -> TimingFields {
        let fields = self.finish(status, end, true);
        self.armed = false;
        fields
    }

    /// Reads the clock once, at `end`, into the values every timing line logs.
    /// Moves the account and model out: a clock is finished exactly once.
    fn finish(&mut self, status: u16, end: Instant, completed: bool) -> TimingFields {
        TimingFields {
            wait_ms: self.send_started.map(|sent| ms_between(self.arrived, sent)),
            throttle_ms: u64::try_from(self.throttle.as_millis()).unwrap_or(u64::MAX),
            upstream_ms: match (self.send_started, self.headers_at) {
                (Some(sent), Some(headers)) => Some(ms_between(sent, headers)),
                _ => None,
            },
            first_byte_ms: self.first_byte_at.map(|at| ms_between(self.arrived, at)),
            total_ms: ms_between(self.arrived, end),
            attempts: self.attempts,
            switched: matches!(
                (self.first_account, self.final_account),
                (Some(first), Some(last)) if first != last
            ),
            status,
            stream: self.stream,
            completed,
            account: self.account.take(),
            model: self.model.take(),
            session: self.session,
        }
    }
}

/// A request held this long by tcr itself (`wait_ms`: selection, holds, throttle,
/// earlier attempts) logs a `slow request` line at INFO. Everything else about a
/// request's time rides on its `request usage` line, or on a DEBUG line, because
/// the daily log is already tens of megabytes and one more INFO line per request
/// is a cost every request pays.
const SLOW_WAIT_MS: u64 = 2_000;

/// One finished request's timing, as logged. See [`RequestTiming`] for what each
/// field measures.
#[derive(Debug)]
struct TimingFields {
    wait_ms: Option<u64>,
    throttle_ms: u64,
    upstream_ms: Option<u64>,
    first_byte_ms: Option<u64>,
    total_ms: u64,
    attempts: u32,
    switched: bool,
    status: u16,
    stream: bool,
    completed: bool,
    account: Option<String>,
    model: Option<String>,
    session: Option<u64>,
}

impl TimingFields {
    /// For a request that logs no `request usage` line (an error, a forwarded
    /// non-2xx, a 2xx that reported no usage): the timing on its own line, at
    /// DEBUG, plus the INFO `slow request` line when tcr held it.
    fn log_alone(&self) {
        tracing::debug!(
            wait_ms = self.wait_ms,
            throttle_ms = self.throttle_ms,
            upstream_ms = self.upstream_ms,
            first_byte_ms = self.first_byte_ms,
            total_ms = self.total_ms,
            attempts = self.attempts,
            switched = self.switched,
            status = self.status,
            stream = self.stream,
            account = self.account.as_deref().unwrap_or("?"),
            model = self.model.as_deref().unwrap_or("?"),
            session = ?self.session,
            completed = self.completed,
            "request timing"
        );
        self.log_if_held();
    }

    /// The INFO exception: tcr itself held this request for [`SLOW_WAIT_MS`] or
    /// more, which is the delay nobody else can see. Logged whichever line carries
    /// the rest, so `slow request` alone finds every one of them.
    fn log_if_held(&self) {
        if self.wait_ms.is_some_and(|wait| wait >= SLOW_WAIT_MS) {
            tracing::info!(
                wait_ms = self.wait_ms,
                throttle_ms = self.throttle_ms,
                upstream_ms = self.upstream_ms,
                first_byte_ms = self.first_byte_ms,
                total_ms = self.total_ms,
                attempts = self.attempts,
                switched = self.switched,
                status = self.status,
                stream = self.stream,
                account = self.account.as_deref().unwrap_or("?"),
                model = self.model.as_deref().unwrap_or("?"),
                session = ?self.session,
                completed = self.completed,
                "slow request"
            );
        }
    }
}

/// A streamed request's clock, owned by the passthrough body. It finishes on
/// drop, because drop is the one place every ending reaches: clean EOF, an
/// upstream error, and the client walking away (axum drops the body mid-poll, so
/// the stream never sees its own end). `ended_at` is set only by the first two,
/// which is what `completed=false` reports.
///
/// The finished fields go to the parser task, which owns the `request usage`
/// line. The parser's channel closes when this body drops, so it is waiting for
/// exactly this send. If the parser is already gone, nobody else will log this
/// request, so the fields are logged here on their own.
struct StreamTiming {
    timing: RequestTiming,
    status: u16,
    done: Option<tokio::sync::oneshot::Sender<TimingFields>>,
}

impl StreamTiming {
    /// The upstream ended the stream (EOF or a transport error): the clock stops
    /// here, and the drop that follows reports `completed=true`.
    fn mark_ended(&mut self) {
        self.timing.mark_ended();
    }

    /// See [`RequestTiming::note_first_byte`].
    fn note_first_byte(&mut self) {
        self.timing.note_first_byte();
    }
}

impl Drop for StreamTiming {
    fn drop(&mut self) {
        let fields = self.timing.finish_stream(self.status);
        match self.done.take() {
            Some(done) => {
                if let Err(fields) = done.send(fields) {
                    fields.log_alone();
                }
            }
            None => fields.log_alone(),
        }
    }
}

/// The relay path's clock, read by the `relay timing` line: `upstream_ms` is the
/// relay's `send()` until headers, `total_ms` is arrival (at [`handle`]) until the
/// body ended (streamed) or the response was built (buffered, or an error).
struct RelayTiming {
    arrived: Instant,
    path: String,
    mode: RelayMode,
    send_started: Option<Instant>,
    headers_at: Option<Instant>,
    stream: bool,
    ended_at: Option<Instant>,
}

impl RelayTiming {
    fn emit(&self, status: u16, end: Instant, completed: bool) {
        let upstream_ms = match (self.send_started, self.headers_at) {
            (Some(sent), Some(headers)) => Some(ms_between(sent, headers)),
            _ => None,
        };
        // DEBUG: a relay is a bookkeeping call tcr never holds, and it must not
        // add a line per request to a log that is already tens of megabytes.
        tracing::debug!(
            upstream_ms,
            total_ms = ms_between(self.arrived, end),
            status,
            stream = self.stream,
            path = %self.path,
            mode = ?self.mode,
            completed,
            "relay timing"
        );
    }
}

/// The streamed relay body's clock; logs on drop for the reason [`StreamTiming`]
/// does.
struct RelayStreamTiming {
    timing: RelayTiming,
    status: u16,
}

impl RelayStreamTiming {
    /// See [`StreamTiming::mark_ended`].
    fn mark_ended(&mut self) {
        self.timing.ended_at = Some(Instant::now());
    }
}

impl Drop for RelayStreamTiming {
    fn drop(&mut self) {
        let completed = self.timing.ended_at.is_some();
        let end = self.timing.ended_at.unwrap_or_else(Instant::now);
        self.timing.emit(self.status, end, completed);
    }
}

/// The catch-all proxy handler. Owns the request's clock: [`handle_request`] fills
/// it in, and a pooled request whose timing was not already logged (on its
/// `request usage` line, or by a streamed body that took the clock) is logged
/// here, with the status the client is about to get.
async fn handle(State(manager): State<Arc<Manager>>, req: Request) -> Response {
    let mut timing = RequestTiming::starting_at(Instant::now());
    let response = handle_request(manager, req, &mut timing).await;
    if timing.armed {
        timing
            .finish(response.status().as_u16(), Instant::now(), true)
            .log_alone();
    }
    response
}

async fn handle_request(
    manager: Arc<Manager>,
    req: Request,
    timing: &mut RequestTiming,
) -> Response {
    let (parts, body) = req.into_parts();
    // A loopback peer is exempt from the api-key gate (see below). Absent the
    // extension (e.g. a direct axum test harness) we treat the client as remote
    // and enforce — fail closed.
    let client_is_loopback = parts
        .extensions
        .get::<ClientAddr>()
        .is_some_and(|a| a.0.ip().is_loopback());
    let method = parts.method;
    let path_and_query = parts
        .uri
        .path_and_query()
        .map_or_else(|| "/".to_string(), |pq| pq.as_str().to_string());
    // The path WITHOUT its query — what the local-route and relay classifiers match
    // on. Matching them against `path_and_query` would let a query string decide
    // routing, and a path is what these rules are actually about.
    let path = parts.uri.path().to_string();
    // A marked ChatGPT tunnel belongs to Codex even outside its backend paths. Those
    // paths relay on the client's credential; the path decides the provider elsewhere.
    let codex_tunnel = parts
        .extensions
        .get::<InterceptedHost>()
        .is_some_and(|host| host.0.eq_ignore_ascii_case(crate::provider::CODEX_HOST));
    let path_provider = Provider::of_path(&path);
    let provider = if codex_tunnel {
        Provider::Codex
    } else {
        path_provider
    };
    let codex_passthrough = match path_provider {
        Provider::Anthropic => codex_tunnel,
        Provider::Codex => false,
    };
    // And the query on its own, for the one consumer that needs the two apart:
    // a request handed to a fallback provider is rebuilt on another Mac, and it
    // used to be rebuilt from the path alone. See `fallback::Ask::query`.
    let parts_query = parts.uri.query().map(str::to_string);
    let req_headers = parts.headers;

    // 1. Auth: when a proxy key is configured, `x-api-key` must match it — EXCEPT
    //    for loopback clients, which are exempt. A localhost personal proxy must
    //    not demand its own key from the local user: `claude` authenticates with
    //    its own OAuth (which we strip and replace with a pooled token) and never
    //    sends the proxy key, which is why loopback is exempt. tcr binds 127.0.0.1 only, so every client is loopback in
    //    practice; the gate still guards a non-loopback client if bound wider.
    if let Some(expected) = manager.proxy_api_key()
        && !client_is_loopback
    {
        let provided = req_headers.get("x-api-key").and_then(|v| v.to_str().ok());
        if !key_matches(provided, expected) {
            return ClientError::new(
                StatusCode::UNAUTHORIZED,
                ErrorKind::Authentication,
                "Missing or invalid x-api-key.",
            )
            .respond(provider);
        }
    }

    // The request key, log mode: decided once, counted when missing, never refused
    // (`crate::request_key`). Where a later wave refuses, it refuses here.
    if let crate::request_key::RequestKeying::Unkeyed(user_agent) =
        request_keying(&parts.extensions, &req_headers, manager.request_key())
    {
        manager.record_unkeyed(
            crate::request_key::Unkeyed::Request(crate::request_key::PathClass::of(&path)),
            user_agent,
            parts
                .extensions
                .get::<ClientAddr>()
                .map_or(0, |addr| addr.0.port()),
        );
    }

    // 1a. Path shape. FIRST of the routing guards, because every one below it — the
    //     `/_tcr/` guard, the host guard, the relay classifier — decides on `path`,
    //     and this is the check that `path` still means upstream what it means here.
    //     `path` is the RAW request target; the upstream URL is parsed from it by
    //     reqwest, which collapses dot segments (and treats `\` as a separator) per
    //     WHATWG. A path where the two disagree is one where every decision below is
    //     made about a URL that never goes on the wire — see [`path_is_ambiguous`]
    //     for the two measured bypasses. Reject rather than reconcile: keeping two
    //     path representations in agreement is correct only while both parsers agree
    //     forever, and no legitimate Anthropic client emits either shape.
    //
    //     Placed AFTER the api-key gate, like the guards below it: a request that
    //     cannot authenticate learns nothing here it would not learn anywhere else.
    if path_is_ambiguous(&path) {
        return ClientError::new(
            StatusCode::BAD_REQUEST,
            ErrorKind::InvalidRequest,
            "Request path must not contain a dot segment or a backslash.",
        )
        .respond(provider);
    }

    // 1b. Everything under `/_tcr/` is OURS. The router matches the registered
    //     routes (today: [`STATUS_PATH`]) BEFORE this catch-all, so any request that
    //     reaches `handle` under the prefix is one we do not serve — and the honest
    //     answer to that is a LOCAL 404, not a forward. Forwarding it would rewrite
    //     a proxy-private path onto api.anthropic.com WITH A POOLED OAUTH BEARER
    //     attached: that is how a typo'd status probe once put Gil's bearer on
    //     `api.anthropic.com/_tcr/status` and burned an account. [`path_is_under`]
    //     covers the bare `/_tcr` too — but only as SPELLED: the compare is
    //     byte-exact and case-sensitive, so `//_tcr/…`, `/_TCR/…` and `/%5ftcr/…`
    //     miss it and are forwarded (measured). See [`LOCAL_PREFIX`] for why that is
    //     a forwarded 404 rather than a hole, and why it is left as it is.
    if path_is_under(&path, LOCAL_PREFIX) {
        return ClientError::new(
            StatusCode::NOT_FOUND,
            ErrorKind::NotFound,
            "Unknown tcr endpoint.",
        )
        .respond(provider);
    }

    // 1c. Host guard: tcr is a credential-injecting reverse proxy for
    //     api.anthropic.com and chatgpt.com, NOT an open forward proxy. Every request
    //     is rewritten to its provider's origin (`manager.upstream_for(provider)`),
    //     DISCARDING its target host: a pooled request with the pooled OAuth Bearer,
    //     a relayed one (every Codex path but inference among them) with the client's
    //     own credential. A plain-HTTP forward-proxy request aimed at a DIFFERENT host
    //     — e.g. an AWS SDK's IMDS probe to 169.254.169.254 that leaked in via a
    //     tool's `HTTP_PROXY` — would otherwise be silently misrouted to Anthropic
    //     (404-flooding the log/TUI, with Gil's Bearer attached). Reject such a
    //     misroute LOCALLY with 421: we neither rewrite it to Anthropic nor
    //     blind-forward it to its real host (that would reopen the SSRF/open-proxy
    //     surface the MITM CONNECT path deliberately closes via the same
    //     allowlist). Placed before body buffering / the rotation loop so a
    //     rejected misroute emits no per-request "serving"/"upstream" log line —
    //     silencing the flood, not relocating it.
    //
    //     Target host = the request-target's authority (absolute-form / forward-
    //     proxy requests carry it) else the `Host` header minus any `:port`. When
    //     BOTH are absent (an origin-form request with no Host, e.g. the direct-axum
    //     test harness) this misroute check is skipped — fail OPEN for the ambiguous
    //     local case, closed only for a host we can positively identify as neither
    //     loopback nor on the MITM policy list (Anthropic or Codex). The provider
    //     check after it skips no request that came through a tunnel.
    let target = target_host(&parts.uri, &req_headers);
    if let Some(host) = target {
        // [`host_is_loopback`] (base-URL mode) reuses the `IpAddr::is_loopback`
        // primitive the client-peer check uses above, and the same derivation the
        // local-endpoint gate runs — one answer to "which host did the client
        // mean", on both paths.
        if !host_is_loopback(host) && !crate::tls::host_allowed(host) {
            tracing::debug!(target_host = %host, "rejected misrouted forward-proxy request");
            return ClientError::new(
                StatusCode::MISDIRECTED_REQUEST,
                ErrorKind::InvalidRequest,
                "This proxy only serves its configured upstream hosts (Anthropic and Codex); refusing to forward a request for a different host.",
            )
            .respond(provider);
        }
    }
    // A decrypted request stays on its CONNECT host. Marked ChatGPT connections may
    // relay paths outside /backend-api/ on the client's credential; they still cannot
    // name a different Host. Other intercepted hosts retain their provider/path guard.
    let refusal = match parts.extensions.get::<InterceptedHost>() {
        Some(InterceptedHost(tunnel)) => {
            let names_another_host = match target {
                Some(host) => !host.eq_ignore_ascii_case(tunnel),
                // No host named, or a `Host` that cannot be read: only an absent one names none.
                None => req_headers.contains_key(HOST),
            };
            if names_another_host {
                Some(tunnel_host_disagreement(tunnel, &path))
            } else if intercepted_host_provider(tunnel) != Some(provider) {
                Some(host_path_disagreement(tunnel, &path, provider))
            } else {
                None
            }
        }
        None => target
            .filter(|host| {
                !host_is_loopback(host) && intercepted_host_provider(host) != Some(provider)
            })
            .map(|host| host_path_disagreement(host, &path, provider)),
    };
    if let Some(message) = refusal {
        return ClientError::new(
            StatusCode::MISDIRECTED_REQUEST,
            ErrorKind::InvalidRequest,
            message,
        )
        .respond(provider);
    }

    // 1c'. A WebSocket upgrade on the Codex inference path is answered here, never forwarded.
    //      Codex opens its stream over a WebSocket by default. A 426
    //      (`upgrade_required_response`) moves it to SSE at once, for the rest of the session,
    //      while any other answer costs retries first, or ends the turn if codex does not retry
    //      it. Forwarding the upgrade would strip `upgrade` (hop-by-hop) and hand the backend a
    //      bare GET it answers with something Codex treats as an error.
    //
    //      Before the relays and before any account is selected, so an upgrade spends no slot,
    //      no throttle token and no identity lookup. And never forwarded as an upgrade either:
    //      the real backend answers one with 101 (measured 2026-10-12), so through tcr it is this
    //      426 that moves the client onto the transport tcr relays.
    match provider {
        Provider::Codex
            if path == crate::provider::CODEX_RESPONSES_PATH
                && wants_websocket_upgrade(&req_headers) =>
        {
            tracing::debug!(
                peer_port = parts
                    .extensions
                    .get::<ClientAddr>()
                    .map(|client| client.0.port()),
                "answered a Codex WebSocket upgrade with 426 so the client falls back to SSE"
            );
            return upgrade_required_response();
        }
        Provider::Codex | Provider::Anthropic => {}
    }

    // 1d. Rotation-bypassing relays. A request bound to the CLIENT's identity
    //     ([`CLIENT_CREDENTIAL_PREFIXES`]) or to a credential exchange that is not
    //     ours ([`CLIENT_TOKEN_REFRESH_PATH`]) must not be re-credentialled, and it
    //     must not spend anything the rotation owns: no `select`, no in-flight slot,
    //     no throttle token, no `record_served`, no pin read or write. Every one of
    //     these used to burn a rotation slot and pollute the LRU key for a request
    //     the pooled account could never have answered.
    //
    //     Deliberately placed AFTER the api-key gate (1), the path-shape guard (1a)
    //     and the host guard (1c): relaying for an unauthenticated caller, on a path
    //     that means something else upstream, or to a host we just refused to forward
    //     to, would each be a new hole. Gate first, then relay.
    //
    //     Before either: the `controlIdentity` check, on the client's own bearer.
    //     It sits here rather than inside the relay because a wrong-account
    //     session must be refused on its bookkeeping calls AND its inference.
    //
    //     Anthropic requests only. The check resolves the bearer through
    //     `/api/oauth/profile` on the ANTHROPIC upstream, and a Codex client's
    //     bearer is a ChatGPT token: asking would hand that token to Anthropic on
    //     every Codex request, and the control account is an Anthropic account
    //     whose identity a ChatGPT login can never match anyway.
    let client_bearer = client_bearer_of(&req_headers);
    match provider {
        Provider::Anthropic => {
            if let Some(bearer) = client_bearer
                && let Some(refusal) = refuse_foreign_client(&manager, bearer).await
            {
                return refusal;
            }
        }
        Provider::Codex => {}
    }
    //     A Codex request that names a workspace routing of its own
    //     ([`names_codex_routing`]) is relayed too, whatever its path, inference
    //     included: its workspace is served by another backend, or under a routing no
    //     pooled account shares, so no pooled bearer may stand in for it.
    //     Counted when that routing is the only reason (`tcr status` reports it): a path
    //     relayed for itself would have been relayed whatever it named.
    let path_relay = if codex_passthrough {
        Some(RelayMode::ClientCredential)
    } else {
        relay_mode(&method, &path, client_bearer.is_some())
    };
    let relay = match path_relay {
        Some(mode) => Some(mode),
        None => match provider {
            Provider::Anthropic => None,
            Provider::Codex => {
                if names_codex_routing(&req_headers) {
                    manager.note_codex_unpooled(crate::now_ms());
                    Some(RelayMode::ClientCredential)
                } else {
                    None
                }
            }
        },
    };
    if let Some(mode) = relay {
        return relay_upstream(
            &manager,
            mode,
            provider,
            method,
            &path_and_query,
            &req_headers,
            body,
            timing.arrived,
        )
        .await;
    }
    // From here on the request is on the pooled path, so every response it ends
    // in (served, forwarded, or synthesized) gets a `request timing` line.
    timing.arm();

    // 2. Buffer the body once so it can be re-sent verbatim on every rotation.
    let Ok(body_bytes) = to_bytes(body, MAX_BODY_BYTES).await else {
        return ClientError::new(
            StatusCode::BAD_REQUEST,
            ErrorKind::InvalidRequest,
            "Failed to read request body.",
        )
        .respond(provider);
    };

    //The body is parsed ONCE, here, into every answer the rest of the handler reads
    // from it: the model, the wire session and its tool events, the account-bound
    // tokens, the affinity key's inputs, the extended-cache-window answer and the
    // account-uuid splice point. See `crate::request_body` for why one walk, and how
    // each answer stays the one its old separate parse gave.
    //
    // The same walk sets every cache breakpoint to the 1-hour window, BEFORE anything
    // below reads the body: the pin's warm window (`note_affinity_ttl`), the ledger's
    // 5m/1h split and the upstream request all see the body as sent. A longer body is
    // fine: `build_upstream_headers` strips content-length. With nothing to change, the
    // client's bytes go out untouched.
    //
    // Anthropic only. The 1-hour window is an Anthropic prompt-cache fact, and a
    // Codex body is forwarded byte for byte: it is usually zstd, and an
    // uncompressed one carrying a `cache_control` key in `tools` would otherwise be
    // re-serialized here.
    //
    // A body that fails to parse yields "no model, no session, no tool events" rather
    // than rejecting the request: these are peeks, never a validation gate.
    let rewrite_ttl = provider.rewrites_cache_ttl() && manager.cache_ttl_rewrite_enabled();
    let request = crate::request_body::RequestBody::parse(body_bytes, rewrite_ttl);
    if let Some((before, after)) = request.rewritten() {
        tracing::debug!(before, after, "cache-ttl: breakpoints rewritten to 1h");
    }
    let body_bytes = request.bytes().clone();

    // The request's target model: constant across the rotation loop, and drives
    // per-model (Fable-aware) account selection below.
    let request_model = request.model().map(str::to_string);

    // F1 (`docs/design/panel-tabs.md`): the Claude Code session id embedded in
    // `metadata.user_id`, plus the `tool_use`/`tool_result` blocks of the newest turn,
    // read independent of whether session affinity is on. The fourth value is NOT
    // observability: `bound` is every account-bound token this conversation carries
    // (advisor ids, thinking signatures, redacted-thinking payloads, and any thread
    // `previous_message_id`) and it decides which account may serve this request, see
    // `crate::bound_tokens`. It covers ALL messages rather than the tail window the tool
    // events use, because the block that binds the conversation was minted on its FIRST
    // turn.
    //
    // `diagnostics.previous_message_id` is deliberately NOT read. Claude Code sends it
    // when it has just given up on a thread and is replaying the full history as a new
    // one (`thread: {type: "create"}`), so it names no state the request depends on, and
    // binding on it pins the RECOVERY request to the account that just failed it. Zero
    // upstream rejections named it over 2026-09-19..22.
    //
    // Server-side thread state, named at the top level or under `thread`, which is where
    // Claude Code sends it: `thread: {type: "continue", previous_message_id}` with only the
    // new messages, is folded into `bound` (`RequestBody::session_facts`). The first
    // sighting of each path is logged (see `note_previous_message_id_path`).
    let (wire_session_id, wire_tool_uses, wire_tool_results, bound) =
        request.session_facts(note_previous_message_id_path);
    // A Codex request names its conversation in the `session-id` header, the key affinity
    // already pins on. Its body is usually zstd, so the peek above read nothing from it, and
    // the header is the one place a Codex turn says which session it belongs to.
    let wire_session_id = match provider {
        Provider::Codex => codex_session_id(&req_headers).map(str::to_string),
        Provider::Anthropic => wire_session_id,
    };
    // Whether THIS request targets a Fable model — the same classification
    // selection uses (`select.rs`). Threaded into every fleet-exhausted 429 so the
    // `retry-after` hint reflects the Fable-scoped weekly gate for a Fable request
    // and ignores it otherwise.
    //
    // Gated on the provider exactly as `select.rs` gates its own two: the Fable
    // weekly bucket is an Anthropic fact, so a Codex request whose model id merely
    // contains "fable" never consults it, here (the bound hold, `select_bound`,
    // every 429 hint) any more than in selection.
    let request_is_fable = provider.has_fable_bucket()
        && request_model
            .as_deref()
            .is_some_and(crate::model::is_fable_model);

    // `tcr run --group <name>` (Phase 1, PREFER semantics only). Read once,
    // constant across the rotation loop — same shape as `request_model` above.
    // An empty header value is treated as absent rather than an empty-string
    // group, matching `compose_group_header`'s own "no inherited value" case.
    let request_group = req_headers
        .get(GROUP_HEADER_NAME)
        .and_then(|v| v.to_str().ok())
        .filter(|g| !g.is_empty())
        .map(str::to_string);

    // The session key pins this connection to one account (opt-in). The extension
    // is present iff session affinity is enabled, so `session_key` is `None` (LRU
    // rotation) by default. When on, key on the most STABLE client identity —
    // for a Codex request its `session-id` header (tier 0, Codex paths only),
    // then x-api-key, then body `metadata.user_id`, then (for a loopback
    // `POST /v1/messages` with neither) a hash of the body's cacheable prefix
    // (`system` + `tools`, see [`stable_session_key`] and [`prefix_session_key`])
    // — so a client that drops and reconnects (new connection key) still maps
    // to the SAME account and keeps its per-account prompt cache warm, and even
    // a client with no stable identity at all still pins on what it would cache
    // identically. Only with NEITHER an identity NOR an in-scope cacheable
    // prefix does the request route UNPINNED (plain LRU): a per-connection key
    // is not a session key — it mints a fresh pin per connection that nothing
    // ever removes, and those ghosts (93% of the live pin map, before this tier
    // existed) both bloat it and inflate the pinned-session counts that drive
    // the migration decision in `select`. `session_kind` records WHICH tier
    // produced the key (stable identity / prefix-derived / unpinned fallback)
    // — DISPLAY provenance only, threaded into `record_served`.
    // The raw per-connection key (present iff session affinity is enabled — see
    // `SessionKey`'s doc-comment), threaded separately from `session_key` above:
    // `conn_key` is the noise-routing grain (`Manager::conn_affinity`, keyed on
    // the CONNECTION), `session_key` is the identity-routing grain (keyed on the
    // client's STABLE identity). The two are deliberately different keys for the
    // same underlying extension — conflating them is exactly the ghost-pin bug
    // `SessionKey`'s doc-comment describes for `session_key`.
    let conn_key = parts.extensions.get::<SessionKey>().map(|k| k.0);
    let (session_key, session_kind) = match parts.extensions.get::<SessionKey>() {
        Some(_) => match stable_session_key(
            &req_headers,
            &request,
            manager.proxy_secrets(),
            &method,
            &path,
            client_is_loopback,
        ) {
            Some((key, kind)) => (Some(key), kind),
            None => (None, SessionKind::Fallback),
        },
        None => (None, SessionKind::Fallback),
    };
    timing.describe(request_model.as_deref(), session_key);

    // Whether THIS request asked Anthropic's cache for the extended 1h window, read
    // off the one parse above. Two consumers: the persisted pin's TTL just below, and
    // the usage ledger's fallback when the response reported no split of its own. A
    // `bool`, so the spawned SSE task carries one rather than the conversation.
    let extended_ttl = request.extended_ttl();

    // The pin's TTL drives nothing on the routing path, only what TTL this
    // session's persisted pin gets at the next restart. With
    // `"sessionAffinity": false` there is no pin to give a TTL to.
    if let Some(key) = session_key {
        manager.note_affinity_ttl(key, extended_ttl);
    }

    // 3. Selection + rotation loop.
    let account_count = manager.account_count();
    // The accounts that could ever serve THIS request: its own provider's, since `eligible`
    // refuses the other provider's first. "Is the rotation spent" (the blind carry below) and
    // "how many accounts are exhausted" (the 429's text) are asked of this, never of the
    // fleet-wide count: in a mixed fleet the other provider's accounts are never tried for
    // this request, so counting them would read a spent Anthropic rotation as unspent.
    let provider_account_count = manager.provider_account_count(provider);
    // Every piece of per-request rotation state, changed only through its methods.
    let mut ladder = Ladder::default();
    // THE BORROWED-REQUEST PATH, and nothing else reaches it: a lease's
    // account-set scope, as the header the lender's own serving leg wrote onto
    // this loopback request (see [`ACCOUNTS_HEADER_NAME`]).
    //
    // "Nothing else reaches it" is ENFORCED by the relay marker beside it, not
    // asserted. This block used to read the header off any inbound request, so
    // any local client could bench accounts by sending it; the sentence above
    // was the whole of the defence and nothing held it up. A request that has
    // not crossed a Mac has no lease and therefore no account-set scope, so
    // its copy of this header is ignored rather than honoured.
    //
    // Applied by BENCHING every account outside the set, seeding `tried`,
    // which every selection call below already honours, rather than by a new
    // argument on the picker. Three properties follow from that, and they are
    // the reason it is written this way:
    //
    //  - it can only narrow. `tried` is the set the picker may not return, so
    //    an out-of-scope account is unreachable by the normal pick, by the
    //    soft fallback, by the 429 rotation and by the revalidation pass, and
    //    no gate is weakened on the way.
    //  - the affinity fast-path honours it too: a pin inside `tried` DIVERTS
    //    (the session keeps its pin, this one request is served elsewhere),
    //    which is exactly what a borrowed request whose session key collided
    //    with a local one needs.
    //  - a set that names no account this fleet carries leaves every account
    //    benched, so the picker answers `None` and the client gets the same
    //    honest 429 an exhausted fleet produces, never an out-of-scope serve.
    //
    // The lender refuses such a scope before a byte is sent
    // (`ScopeRestriction::Unenforceable`), so this is the backstop and not the
    // enforcement point.
    if let Some(labels) = req_headers
        .get(ACCOUNTS_HEADER_NAME)
        .filter(|_| req_headers.contains_key(RELAYED_HEADER_NAME))
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|label| !label.is_empty())
                .map(str::to_string)
                .collect::<Vec<String>>()
        })
        .filter(|labels| !labels.is_empty())
    {
        for idx in 0..account_count {
            let inside = manager.account_name(idx).is_some_and(|name| {
                tcr_peer_wire::sanitize_label(&name).is_ok_and(|label| labels.contains(&label))
            });
            if !inside {
                ladder.bench(idx);
            }
        }
        tracing::debug!(
            accounts = %labels.join(","),
            benched = ladder.benched(),
            of = account_count,
            "account-set scope: this request may be served only by the accounts it names"
        );
    }
    // A conversation whose history carries ACCOUNT-BOUND state is served by the account that
    // minted that state for as long as that account can hold it.
    //
    // An advisor result decrypts only on the org that produced it, and the thread state behind a
    // `previous_message_id` lives on that account alone: another account answers the first such
    // request with a 400 or a 404 (measured 2026-09-20..22 over 173k calls, 8,322 advisor 400s
    // and 66 thread-state 404s, each cluster starting where a pinned conversation was diverted;
    // 4,420 diverts in the same window). So while the bound account can serve, a sibling is the
    // wrong answer, and a short wait on the bound account is the right one.
    //
    // A `thinking` block is different, and it depends on the model. The same September window
    // counted 170 thinking 400s; measured 2026-10-10 by replaying one block unchanged on another
    // account, the current models answer 200 instead:
    //
    //  - Opus 5.5 keeps the block on any account, in any org. Its hold earns its keep through the
    //    warm prompt cache, which is what `switch_warning` tells the client a move costs.
    //  - Sonnet 5.5 and Haiku 5.5 keep it on another seat in the minting account's org, and on an
    //    account in another org DROP it silently: a 200, no error, the earlier reasoning gone,
    //    and nothing for the client to recover from. Hence the release below prefers a same-org
    //    seat for those two (`same_org_release`, `crate::model::thinking_is_org_bound`).
    //
    // The advisor and thread-state rejections are not fatal. Claude Code recovers from each in
    // one extra round trip: it strips the advisor blocks, or replays the full history as a new
    // thread (see `crate::manager::bound_tokens`'s module doc for where that was read). So the
    // hold ends where the bound account stops being able to serve soon — gone, `rejected`, or
    // held past the cache's life (`Manager::bound_account_holds`). Past that point a hold is a
    // 429 on every turn until the account resets, possibly for days, and `/compact` does not
    // clear it, because the thread id survives the compaction (2026-09-22: one `rejected`
    // account held one conversation at 6-11 refusals a minute).
    //
    // Enforced by BENCHING every other account into `tried`, exactly as the account-set scope
    // above does and for the same three reasons — it can only narrow, the affinity fast-path
    // honours it (so a request whose pin is not the bound account is held here rather than
    // diverting, and the pin itself is left alone), and if the bound account is gone from this
    // fleet every account ends up benched and the client gets the same honest 429 an exhausted
    // fleet produces.
    //
    // Two rungs name the bound account, because a token is not always knowable:
    //
    //  1. A token this process minted names its account outright.
    //  2. No token resolves, but the history IS bound and the session has a pin: the pin. This
    //     is every conversation older than this build, and the pin is where that history was
    //     made as long as it never diverted.
    //
    // Neither: today's behaviour, so the rule never blocks a conversation it knows nothing
    // about. Either, with an account that cannot hold it: released, and logged, so the one
    // rejection that follows can be traced to this decision.
    //
    // `bound_to` carries the one account this request may use down to the terminals, which
    // must answer for THAT account: the wait they size and the `retry-after` they advertise
    // are its own, and a fallback provider (a peer's credential) is refused outright, since
    // it is one more account that cannot read this history.
    let minted = manager.bound_token_account(&bound.tokens);
    if let Some((_, bound_kind)) = minted
        && manager.bound_tokens_disagree(&bound.tokens)
    {
        tracing::warn!(
            tokens = bound.tokens.len(),
            kind = bound_kind.as_str(),
            "bound tokens in one conversation name different accounts; taking the newest mint"
        );
    }
    let session_pin = session_key.and_then(|key| manager.affinity_pin(key));
    let bound_candidate: Option<(usize, &'static str)> = match (minted, session_pin) {
        (Some((bound_idx, bound_kind)), _) => Some((bound_idx, bound_kind.as_str())),
        (None, Some(pinned_idx)) if bound.carries_bound_history => {
            Some((pinned_idx, "session-pin"))
        }
        (None, _) => None,
    };
    // A bound or pinned account of the OTHER provider can never serve this request: its
    // credential belongs to the other upstream. Held there, every account of this request's
    // provider was benched below and `Manager::select_bound`, which checks no provider, then
    // served the request on that account's bearer against this request's origin. Dropped
    // instead, so the request takes the ordinary pick, whose provider check diverts it off
    // such a pin without re-keying the session (`select_with_group`).
    let bound_candidate = bound_candidate.filter(|&(bound_idx, via)| {
        let same_provider = manager.provider(bound_idx) == Some(provider);
        if !same_provider {
            tracing::debug!(
                account = manager.account_name(bound_idx).as_deref().unwrap_or("?"),
                kind = via,
                provider = provider.as_str(),
                "bound history names an account of the other provider; not holding the request there"
            );
        }
        same_provider
    });
    // A COLD conversation whose only bound state is thinking that survives a move is not held.
    //
    // For Opus 5.5 a thinking block reads on any account (the list above), so its hold buys one
    // thing only: the warm prompt cache. Once the conversation has been idle past the cache's
    // life (`Manager::bound_history_is_cold`) that is gone, and holding it anyway kept it on the
    // account it started on for up to `BOUND_TOKEN_TTL_MS`, a day, over the weekly-reset order
    // every other new pick follows. Reported 2026-11-07: two Ultra accounts, the one resetting
    // in 3d 14h at 95% carrying the day's traffic while the one resetting in 11h sat at 16%.
    //
    // Never for advisor results or thread state (`carries_account_only_state`), which no other
    // account can read, nor for a model whose thinking is dropped across organizations, nor for
    // a request whose model is unknown: those keep today's hold.
    let bound_candidate = bound_candidate.filter(|&(bound_idx, via)| {
        let thinking_travels = request_model
            .as_deref()
            .is_some_and(|model| !crate::model::thinking_is_org_bound(model));
        let release = thinking_travels
            && !bound.carries_account_only_state
            && manager.bound_history_is_cold(
                &bound.tokens,
                session_key,
                crate::now_ms(),
                manager.warm_window(session_key),
            );
        if release {
            tracing::info!(
                account = manager.account_name(bound_idx).as_deref().unwrap_or("?"),
                kind = via,
                model = request_model.as_deref().unwrap_or("?"),
                "bound history is cold and its thinking travels; not holding it, the pick decides"
            );
        }
        !release
    });
    let mut bound_to: Option<usize> = None;
    if let Some((bound_idx, via)) = bound_candidate {
        let now = OffsetDateTime::now_utc();
        // A session already told this conversation is moving off `bound_idx` moves, even if the
        // account has crept back inside the hold since: the warning said the retry moves it.
        let told =
            session_key.is_some_and(|key| manager.switch_warned(key, bound_idx, crate::now_ms()));
        if !told
            && manager.bound_account_holds(
                bound_idx,
                now,
                request_is_fable,
                request_group.as_deref(),
                manager.warm_window(session_key),
            )
        {
            bound_to = Some(bound_idx);
            for idx in 0..account_count {
                if idx != bound_idx {
                    ladder.bench_for_hold(idx);
                }
            }
            if via == "session-pin" {
                if session_key.is_some_and(|key| manager.first_bound_history_hold(key)) {
                    tracing::info!(
                        account = manager.account_name(bound_idx).as_deref().unwrap_or("?"),
                        "bound history, no minted token known: holding session to its pin"
                    );
                }
            } else if session_pin.is_some_and(|pinned| pinned != bound_idx) {
                tracing::info!(
                    account = manager.account_name(bound_idx).as_deref().unwrap_or("?"),
                    kind = via,
                    "bound token pins request to the account that minted it"
                );
            }
        } else {
            // Told first, once: the client's retry of this 429 is the request that moves.
            if let Some(warning) = switch_warning(
                &manager,
                provider,
                session_key,
                bound_idx,
                now,
                request_is_fable,
                request_group.as_deref(),
            ) {
                return warning;
            }
            tracing::info!(
                account = manager.account_name(bound_idx).as_deref().unwrap_or("?"),
                kind = via,
                gated = ?manager
                    .exhaustion_hint_for_account(now, request_is_fable, bound_idx)
                    .gated,
                "bound history's account cannot hold it; releasing the request to the fleet"
            );
            // A same-organization sibling a released conversation is sent to first
            // (`same_org_release`), already selected, so it seeds the ladder's next attempt.
            // Nothing was benched for a hold on this path, so the release re-admits nothing.
            ladder.release_to(|tried| {
                same_org_release(
                    &manager,
                    bound_idx,
                    tried,
                    request_model.as_deref(),
                    |narrowed| {
                        manager.select_with_group(
                            narrowed,
                            now,
                            request_model.as_deref(),
                            session_key,
                            &path,
                            conn_key,
                            request_group.as_deref(),
                        )
                    },
                )
            });
        }
    }
    // Bound the total attempts so per-account 401/429 retries can never loop.
    let max_attempts = max_attempts_for(account_count);
    // The same-account transport retry (`Ladder::spend_transport_retry`) is gated on the
    // error KIND. The transport-failure branch below is THREE arms,
    // evaluated in this order — read them as narrowing scope, machine → connection
    // → account:
    //
    //  1. OFFLINE (`is_offline_error`): name resolution failed, so the fault is
    //     the MACHINE. Every account resolves the same hostname, so there is
    //     nothing to bench and nowhere to rotate to. Holds the pin, waits, retries
    //     the same account, and after `MAX_OFFLINE_WAITS_PER_REQUEST` answers 503.
    //  2. SAME-ACCOUNT RETRY: a non-CONNECT failure, i.e. a pooled
    //     CONNECTION died. Up to `MAX_SAME_ACCOUNT_TRANSPORT_RETRIES` per account
    //     per request (the rationale is on `Ladder`'s field). Deliberately not taken for a
    //     CONNECT failure — there was no connection to evict, nothing is
    //     refreshed by trying again, and the retry only buys a second connect
    //     timeout on a route already not answering.
    //  3. ROTATE: everything else — a route to a resolvable host that is refused,
    //     blackholed or timing out. That IS evidence about this route, so bench the
    //     account and let the rotation do its job.

    let wants = Wants {
        model: request_model.as_deref(),
        session_key,
        path: &path,
        conn_key,
        group: request_group.as_deref(),
        is_fable: request_is_fable,
    };
    for _ in 0..max_attempts {
        let now = OffsetDateTime::now_utc();
        let idx = if let Some(idx) = ladder.pick(&manager, wants, now) {
            idx
        } else {
            // A RESERVED group is strict: this request may only ever be
            // served by one of its members, so every answer below — the
            // wait it sizes, the revalidation it might attempt, the 429 it
            // finally returns — has to be scoped to that group rather than
            // to a fleet this request is not allowed to touch.
            //
            // Recomputed here rather than hoisted alongside `request_group`
            // at the top of the handler: `reserved` hot-reloads, so
            // `tcr group unreserve <g>` releases an already-stalled request
            // on its next pass through this arm instead of after a restart.
            // `is_group_strict`, not `is_group_reserved`: the THIRD
            // door out of a strict group, which a measurement found
            // open here and on the exhausted path below. The
            // picker holds a request strictly for `reserved || !spill`
            // and this arm asked only about `reserved`, so for a
            // group that is neither reserved nor a spill group, a
            // lease's scope is exactly one, every answer below was
            // sized against a fleet the request was never allowed to
            // touch. One rule, one function: see
            // `Manager::is_group_strict`.
            let strict_group = request_group
                .as_deref()
                .filter(|g| manager.is_group_strict(g));
            // What a fallback provider would be told, built once for the
            // three terminals below. It carries the client's own
            // headers, MINUS every credential, and `Ask::scrubbed` is what
            // removes them: `build_upstream_headers`, the only code in
            // this file that strips a client's `authorization` and
            // substitutes a pooled token, is 215 lines downstream of
            // here, so the scrub cannot be left to it.
            //
            // They are carried rather than dropped because a relayed
            // request with no `anthropic-version` and no
            // `content-type` is answered 400 by the API: an `Ask` that
            // carried nothing could only produce an answer the client
            // cannot use, and the borrower served that 400 as if a
            // lender had answered it.
            //
            // `None` for a path no provider may ever be offered, and
            // that refusal is HERE rather than only inside the
            // provider: the `Ask` clones the whole request body, so
            // asking first means a client-credential body is never
            // copied for a relay that cannot happen. The provider
            // keeps the identical check as its own first act, a
            // provider is written by whoever adds the next one, and
            // this seam cannot rely on that, so this is the cheap
            // half of a defence that exists twice on purpose.
            //
            // `None` for a non-POST for the same reason and one more of
            // its own: a relayed request is a POST or nothing
            // (`peer::serve::SERVE_METHOD`), and the borrower used to
            // write the literal `"POST"` into the frame whatever its
            // client had sent, so a `GET` was POSTed on somebody
            // else's account, out of a lease. The lender refuses that
            // frame now, but only after it has crossed a host
            // boundary; refusing here means a non-POST this fleet
            // cannot serve gets the SAME answer it got before any of
            // this existed, from `exhausted_or_fallback`'s terminal.
            // `RELAYED_HEADER_NAME` is the one-hop rule: a request
            // that has already crossed one Mac is served here or
            // refused here, never borrowed onward. See that constant.
            let fallback_ask = (method == Method::POST
                && crate::peer::serve::serve_is_allowed_for_path(&path)
                && !req_headers.contains_key(RELAYED_HEADER_NAME))
            .then(|| fallback::Ask {
                path: &path,
                // The query alongside the query-stripped path, never
                // folded into it: the classifiers above match on the
                // path, and a borrowed request that arrived upstream
                // with its parameters dropped answered a different
                // question from the one the client asked.
                query: parts_query.as_deref(),
                method: method.as_str(),
                model: request_model.as_deref(),
                group: request_group.as_deref(),
                affinity: session_key,
                tried_local: ladder.benched(),
                body: body_bytes.clone(),
                headers: fallback::Ask::scrubbed(&req_headers),
            });
            if let Some(answer) = ladder.verdict(
                &manager,
                provider,
                request_model.as_deref(),
                now,
                bound_to,
                strict_group,
            ) {
                return answer;
            }
            // A cold shared-limiter burst parks the whole fleet for ~15-20s.
            // Rather than telling the client "all exhausted" for a transient
            // blip, wait out the soonest un-park ONCE and retry select — the
            // JS original's inline-wait-retry (L3). Real exhaustion (quota
            // windows / long holds) has soonest_free >> the ceiling →
            // soft_wait_secs returns None → fall through to the honest 429.
            if let Some(secs) = soft_wait_secs(
                match (bound_to, strict_group) {
                    // Bound history: the one account it may use is the only
                    // clock that matters, for the same reason as the group arm.
                    (Some(idx), _) => {
                        manager
                            .exhaustion_hint_for_account(now, request_is_fable, idx)
                            .retry_after
                    }
                    // Size the wait by when a MEMBER frees. The fleet-wide
                    // hint would be won by any unrelated account un-gating
                    // sooner, spending this request's one-shot soft-wait on
                    // a recovery it cannot use and 429-ing while its own
                    // member was still seconds away.
                    //
                    // Both arms fold only this request's PROVIDER's accounts,
                    // for the same reason: an account of the other provider
                    // freeing sooner is a recovery this request cannot use.
                    (None, Some(group)) => {
                        manager.retry_after_hint_for_group(now, request_is_fable, provider, group)
                    }
                    (None, None) => manager.retry_after_hint(now, request_is_fable, provider),
                },
                ladder.soft_waited_exhaustion(),
            ) {
                ladder.spend_soft_wait();
                // Desync concurrent waiters: without this, every request
                // sleeping the same integer-ceil `secs` wakes on the SAME
                // second and races for the single earliest-freeing account,
                // re-synchronizing the herd NO_GUIDANCE_JITTER (:555) spreads
                // and re-bursting that account. Waking `jitter` seconds later
                // still serves (the account already freed at `secs`); worst
                // case adds NO_GUIDANCE_JITTER_MAX_SECS (5s) → total ≤ 25s.
                let jitter =
                    (i64::from(now.nanosecond()) % (NO_GUIDANCE_JITTER_MAX_SECS + 1)) as u64;
                let wait = secs + jitter;
                tracing::info!(
                    wait_secs = wait,
                    "fleet transiently parked — soft-waiting once before exhausted"
                );
                tokio::time::sleep(Duration::from_secs(wait)).await;
                // Re-admit exactly what the sleep was timed for.
                // `Transient429::Park` benches an account TWICE — a
                // hold via `mark_rate_limited` AND an entry in
                // `tried` — and both `pick_eligible` and
                // `pick_least_loaded` test `tried` BEFORE they ever
                // evaluate eligibility. So clearing only the hold
                // leaves the account this sleep was timed for still
                // skipped: select() returns `None` a second time, the
                // one-shot guard blocks another wait, and the wait
                // buys nothing. Dropping the now-expired parks from
                // `tried` is what makes the recovery path actually
                // reach the account it was built to preserve.
                //
                // Precision matters more than reach here: an account
                // whose hold is still running stays parked, and one
                // benched for any NON-hold reason was never in
                // `parked_transient` to begin with.
                let (readmitted, still_parked) =
                    ladder.readmit_expired_parks(&manager, crate::now_ms());
                tracing::info!(
                    readmitted,
                    still_parked,
                    "soft-wait over — re-admitted the accounts whose hold expired"
                );
                continue; // re-run select(); the re-admitted accounts are eligible again
            }
            // A hard account lock has no failover. Normal selection
            // returned None because that one account already failed
            // this request. Revalidation must not escape the lock and
            // send the request through another pooled credential.
            //
            // **And neither may a lease.** This arm was changed to
            // `exhausted_or_fallback` when the fallback seam landed,
            // which consults a provider that never reads
            // `locked_account_name`: a request the operator pinned to
            // ONE account was then served on a PEER's credential the
            // moment that account 429'd, which is the same escape the
            // revalidation serve is refused for and a wider one, the
            // credential is not even this Mac's. The refusal is
            // written once, inside `exhausted_or_fallback`, so it
            // holds for this arm and for the two below it rather
            // than only for the arm somebody remembered.
            if manager.locked_account_for(provider).is_some() {
                return exhausted_or_fallback(
                    &manager,
                    now,
                    provider_account_count,
                    request_is_fable,
                    provider,
                    strict_group,
                    bound_to,
                    fallback_ask.as_ref(),
                )
                .await;
            }
            // Not a transient park — the whole fleet reads over the
            // SOFT switch threshold. Before synthesizing a 429, try a
            // last-resort revalidation serve on the least-utilized
            // account Anthropic still allows (default ON; disable via
            // top-level `revalidationServe: false`). An upstream 200
            // refreshes stale quota; a 429 arms a real hold via the
            // existing inline-429 handling. `None` (throttled, or
            // nothing servable) keeps the honest exhausted 429.
            // `strict_group.is_none()` guards the THIRD spill door,
            // and the least visible one. `select_revalidation`
            // passes `group: None` to its hard gate, which by
            // `Manager::reserved_blocks` EXCLUDES every account
            // carrying a reserved group — so for a strict request
            // it can only ever return a NON-member. Left ungated it
            // would hand the request to precisely the account the
            // reservation exists to keep it off, moments after the
            // strict arm in `select_with_group` refused to. A
            // reserved group falls straight through to the honest
            // 429 instead.
            //
            // HELD HISTORY IS NOT A FLEET QUESTION. A request held to
            // its bound account has every other account benched, so
            // "the whole fleet is over the threshold" here means only
            // "that one account is", and the fleet's revalidation
            // window (one serve per 2 s, shared by every session) is
            // the wrong gate for it: the turn that lost the race got
            // a 429 naming the account's 5-hour window, which Claude
            // Code shows as "You've hit your session limit"
            // (2026-09-24). `bound_account_holds` already decided the
            // soft threshold does not release this conversation, so
            // the serve agrees with it: `Manager::select_bound`.
            //
            // And when the held account went out DURING this request
            // (an upstream rejection on it just turned
            // `bound_account_holds` to no), this is the same move the
            // top of the handler makes on a later turn, so it gets the
            // same answer now instead of a 429 that waits for that
            // turn: the warning once, then the fleet.
            if let Some(bound) = bound_to
                && !manager.bound_account_holds(
                    bound,
                    now,
                    request_is_fable,
                    request_group.as_deref(),
                    manager.warm_window(session_key),
                )
            {
                if let Some(warning) = switch_warning(
                    &manager,
                    provider,
                    session_key,
                    bound,
                    now,
                    request_is_fable,
                    request_group.as_deref(),
                ) {
                    return warning;
                }
                tracing::info!(
                    account = manager.account_name(bound).as_deref().unwrap_or("?"),
                    "bound history's account went out during the request; \
                             releasing the request to the fleet"
                );
                // Re-admit only what the hold benched: a lease's
                // account-set scope stays benched (see
                // `Ladder::release_hold`), and so does `bound`, which
                // failed this request.
                ladder.release_to(|tried| {
                    same_org_release(
                        &manager,
                        bound,
                        tried,
                        request_model.as_deref(),
                        |narrowed| {
                            manager.select_with_group(
                                narrowed,
                                now,
                                request_model.as_deref(),
                                session_key,
                                &path,
                                conn_key,
                                request_group.as_deref(),
                            )
                        },
                    )
                });
                bound_to = None;
                continue;
            }
            match ladder.last_resort(&manager, wants, now, bound_to, strict_group) {
                Some(idx) => idx,
                None => {
                    return exhausted_or_fallback(
                        &manager,
                        now,
                        provider_account_count,
                        request_is_fable,
                        provider,
                        strict_group,
                        bound_to,
                        fallback_ask.as_ref(),
                    )
                    .await;
                }
            }
        };

        // RAII in-flight accounting for per-account pacing: bump this account's
        // concurrency count and stamp its last-served time NOW, releasing the slot
        // on EVERY exit of this iteration — rotate (`continue`), terminal `return`,
        // or panic-unwind. On rotation the old account's guard drops here and a
        // fresh one is taken for the new idx next iteration, so the counter tracks
        // true concurrent load and can never leak to strand an account. Inert when
        // pacing is unconfigured (the count is simply never read by `eligible`).
        let in_flight = manager.enter_in_flight(idx);

        // Refresh the token if it is hard-expired (coalesced across concurrent
        // requests). If the refresh proved the credential dead, skip now.
        manager.ensure_fresh(idx).await;
        if manager.account_status(idx) == Some(AccountStatus::Error) {
            ladder.bench(idx);
            continue;
        }
        let Some(token) = manager.access_token(idx) else {
            ladder.bench(idx);
            continue;
        };
        // The upstream headers for THIS account, built once and handed to every route this
        // attempt can take (the exit lock below, the direct send, the blind carry), so the
        // three can never disagree about whose credential and whose identity travel together.
        // Codex: the pooled bearer plus the serving account's `chatgpt-account-id`, replacing
        // the client's. codex sends its bearer with its own account's id (`openai/codex`
        // f82e365, `model-provider/src/bearer_auth_provider.rs:31-47`), so tcr keeps the two
        // one account's pair; what the backend makes of a mismatched pair is unmeasured.
        // That id is the row's workspace, its `org_uuid`, else its `account_uuid` for a row
        // saved before the two were split (`Manager::codex_account_id`).
        let upstream_headers = match provider {
            Provider::Anthropic => build_upstream_headers(&req_headers, &token),
            Provider::Codex => {
                let id = manager.codex_account_id(idx);
                build_codex_upstream_headers(&req_headers, &token, id.as_deref())
            }
        };
        // The body THIS account sends, built once beside its headers and for the same reason:
        // the exit lock, the direct send and the blind carry all take this one value, so a
        // carried request is the same request the direct path would have sent.
        //
        // `None` for a GET or a HEAD, which carry no body. Anthropic: the body's
        // `metadata.user_id.account_uuid` is rewritten to the serving account's own UUID
        // (rotation changes the account each attempt, so this is recomputed from the pristine
        // `body_bytes` per attempt), at the splice point the one parse found; the patch is
        // same-length, so content-length is unaffected, and `build_upstream_headers` strips
        // content-length regardless. Absent a configured 36-char UUID, the body is sent
        // unchanged. Codex: the identity is the header set above; the body is opaque (usually
        // zstd). Every no-change path forwards the buffered `Bytes` handle by O(1) refcount
        // clone; only a genuine differing-UUID rewrite allocates.
        let upstream_body: Option<Bytes> =
            (method != Method::GET && method != Method::HEAD).then(|| match provider {
                Provider::Anthropic => match manager.account_uuid(idx) {
                    Some(uuid) if uuid.len() == 36 => request.for_account(&uuid),
                    _ => body_bytes.clone(),
                },
                Provider::Codex => body_bytes.clone(),
            });
        // **The exit lock, before the direct attempt and not inside its failure
        // arm.** An account the operator locked to one Mac leaves from that Mac
        // on EVERY request, working direct path or not: the whole point is that
        // the origin sees one address for it. So this is consulted here rather
        // than beside the blind-carry offer far below, and it never reads a
        // transport failure, because on this line nothing has been attempted
        // yet.
        //
        // It sits just BELOW `access_token` and not above it because a carried
        // request is the same request on the same credential, and this
        // account's Bearer is built by `build_upstream_headers`, which is the
        // one place that scrub lives.
        //
        // Inert for every account that is not pinned: one config read answers
        // `TakeTheDirectPath` before a file, a key or a socket is touched. The
        // decision, the refusal and the one fallback log line are all in
        // `peer::tunnel::egress`.
        let already_paced = match crate::peer::tunnel::egress::pinned_egress(
            crate::peer::tunnel::egress::PinnedAttempt {
                manager: &manager,
                account_idx: idx,
                method: &method,
                path_and_query: &path_and_query,
                headers: upstream_headers.clone(),
                body: upstream_body.clone(),
                peers_path: &crate::peer::tunnel::egress::peers_path(),
                session_key,
                session_kind,
                wire_session_id: wire_session_id.as_deref(),
                model: request_model.clone(),
                tool_uses: &wire_tool_uses,
                tool_results: &wire_tool_results,
            },
        )
        .await
        {
            // `paced` and not a second `throttle_send`: a pin that could not
            // be honoured has already waited on this account's own bucket
            // inside the exit lock, and pacing it again below made one client
            // request spend two slots on the one account an operator had
            // explicitly asked to shape.
            crate::peer::tunnel::egress::PinnedEgress::TakeTheDirectPath { paced } => paced,
            crate::peer::tunnel::egress::PinnedEgress::Answered(response) => return response,
        };
        // This account's OWN client — fetched INSIDE the loop, keyed on the idx
        // this iteration is actually about to serve. Hoisting this above the
        // loop (as it used to be) reused whichever account was selected FIRST
        // for every later rotation attempt too, which is the bug
        // `AccountRuntime::http` exists to fix: every account collapsing onto
        // one shared connection pool regardless of which one is serving.
        let Some(http) = manager.http_client(idx) else {
            ladder.bench(idx);
            continue;
        };

        // Build the upstream request: clone headers minus hop-by-hop / auth /
        // encoding, then set OUR bearer (the client's authorization was dropped).
        // The origin is the request's provider's: a Codex request never reaches
        // the Anthropic upstream, whichever account served it.
        let url = format!("{}{}", manager.upstream_for(provider), path_and_query);
        let mut builder = http
            .request(method.clone(), &url)
            .headers(upstream_headers.clone());
        // reqwest's `Body: From<Bytes>` is O(1): no full-body copy per attempt.
        if let Some(body) = upstream_body.clone() {
            builder = builder.body(body);
        }

        // Fetch the serving account's name once per iteration — reused by the
        // transport-failure warning, the upstream-response line, and, on the
        // terminal path, by push_log (was two read-locks + clones back-to-back).
        // Hoisted ABOVE the send so the transport-failure arm can name the account
        // it failed on — that arm used to log nothing at any level.
        let account_name = manager.account_name(idx);

        // Outbound throttle: a PER-ORGANIZATION bucket (the real limiter — Anthropic
        // limits per org) plus a much looser fleet-wide ceiling. Inert unless
        // configured. Placed after account selection/token so only real sends consume
        // a slot, and so the per-org bucket can be keyed off the account actually
        // about to be hit; both the 401 force-refresh retry and the transient-429
        // retry loop back here, so every retry is paced against whichever account it
        // rotated to.
        if !already_paced {
            let throttle_started = Instant::now();
            manager.throttle_send(&path, idx).await;
            timing.note_throttle(throttle_started);
        }

        timing.note_send(idx, account_name.as_deref());
        // The attempt, from the send to the last status arm, evaluates to the one `Step` it
        // decided, and `Ladder::apply` after it is the only place the ladder moves. An arm that
        // changed the ladder and jumped by itself is how a park once forgot its second set.
        let step = 'attempt: {
            let resp = match builder.send().await {
                Ok(resp) => {
                    timing.note_headers();
                    resp
                }
                Err(err) => {
                    // Transport failure is not proof of a bad credential, so nothing
                    // here ever disables an account — the three arms differ only in
                    // whether this request retries in place, rotates, or gives up.
                    // Their order and rationale are at `transport_retried` above. The
                    // `reqwest::Error` used to be discarded by a `let Ok(..) else`,
                    // so a 502 assembled out of these failures had no line anywhere to
                    // attribute it to; `is_connect` / `is_timeout` separate "never
                    // reached the host" from "the host went quiet mid-request".
                    ladder.transport_failed();
                    manager
                        .record_upstream_error(provider, crate::observations::ErrorKind::Transport);
                    // **Blind peer egress, the one offer per request.**
                    //
                    // Four conjuncts, and the first two are what stop a carry
                    // RESENDING a request an upstream may already have acted on.
                    // `err.is_connect()` alone was not enough, and that was the
                    // CRITICAL a measurement found: it describes only THIS
                    // attempt, so a request whose earlier attempt died past the
                    // connect phase (`unknown_outcome_transport_failure`, set
                    // below) and whose next one failed to connect satisfied it and
                    // carried a POST that may have been received. So:
                    //
                    //  - `!unknown_outcome_transport_failure`: no attempt in this
                    //    request has ever been written onto a live connection.
                    //  - `every_attempt_transport_failed(..)`: no attempt ever
                    //    reached an upstream at all. An upstream answer, a 429, a
                    //    403, anything, is proof the request left this box, and it
                    //    is a fact about the REQUEST, not about this attempt.
                    //  - `err.is_connect() || is_offline_error(&err)`: this attempt
                    //    died before or at connect.
                    //  - `rotation_is_spent`: the ladder has no untried account
                    //    left to rotate to, OR the failure is machine-level
                    //    (the resolver), where rotating cannot help because every
                    //    account resolves the same name. Offering on the FIRST
                    //    connect failure of a multi-account fleet is what let a
                    //    carry run while an account that would have answered had
                    //    not been tried yet.
                    //
                    // Position follows from that last conjunct: after rotation,
                    // and still ABOVE the resolver arm's fixed six-second wait, so
                    // an offline machine is offered a carry before it sleeps
                    // rather than after. One call site, bounded once per request by
                    // `peer_egress_tried`.
                    //
                    // Inert by construction when the mesh is not set up: no peers
                    // file, `via off`, or no pinned Mac with an address all answer
                    // `None` before anything is dialled, and then this arm carries
                    // on to exactly the behaviour it had before. Nothing about
                    // `is_offline_error`, `every_attempt_transport_failed` or
                    // either terminal status is widened.
                    let machine_level = is_offline_error(&err);
                    let nothing_ever_left_this_box =
                        !ladder.tally().unknown_outcome_transport_failure
                            && (err.is_connect() || machine_level)
                            && every_attempt_transport_failed(
                                ladder.tally().transport_failures,
                                ladder.tally().upstream_responses,
                            );
                    // `tried` never contains `idx` yet (it is inserted on the way
                    // out of this arm), so this account plus the benched ones is
                    // the whole of this request's provider's accounts exactly when
                    // the ladder is out of moves. A request narrowed by the
                    // account-set scope header counts its own benched accounts here
                    // too, which is right: they are not rotatable for this request
                    // either.
                    //
                    // Both sides count only the REQUEST's provider: the other
                    // provider's accounts are never rotatable for it, so with the
                    // fleet-wide count a mixed fleet read one Anthropic ladder short
                    // of spent per Codex account, and an Anthropic request was never
                    // offered the carry it gets on the same fleet without them. They
                    // are filtered out of `tried` too, because the scope header
                    // benches every account outside its set, whatever its provider.
                    let tried_of_provider =
                        ladder.benched_where(|i| manager.live_provider(i) == Some(provider));
                    let rotation_is_spent =
                        machine_level || tried_of_provider + 1 >= provider_account_count;
                    // Anthropic only: Codex stays off the peer mesh in this version (it is
                    // neither lent nor carried), and a carry offers the request to a
                    // peer's gateway as surely as a lease does. A Codex request keeps the
                    // answer it had before the mesh existed.
                    if !ladder.peer_egress_tried()
                        && nothing_ever_left_this_box
                        && rotation_is_spent
                        && provider.on_peer_mesh()
                    {
                        ladder.spend_peer_egress();
                        let peers_path = crate::peer::tunnel::egress::peers_path();
                        let carry = crate::peer::tunnel::egress::retry_through_peer(
                            crate::peer::tunnel::egress::CarriedRequest {
                                url: &url,
                                method: &method,
                                headers: upstream_headers.clone(),
                                body: upstream_body.clone(),
                                peers_path: &peers_path,
                            },
                        )
                        .await;
                        // A GATEWAY THAT TOOK THE CARRY AND FAILED IS AN ANSWER,
                        // never a fall-through. The ladder below this arm sleeps
                        // and retries the same account, or benches it and rotates,
                        // and either way sends a request the splice may already
                        // have put on the wire a second time. Nothing else on this
                        // path can say so: `unknown_outcome_transport_failure` is
                        // written by the direct attempt's own error arm, which a
                        // carry never reaches, so the terminal answer was a 503
                        // the client is built to retry. Same typed outcome as the
                        // pinned sibling, same 502 with `x-should-retry: false`.
                        if let crate::peer::tunnel::egress::ViaCarry::TakenAndFailed(gap) = &carry {
                            break 'attempt Step::Answer(
                                crate::peer::tunnel::egress::delivered_unknown_response(gap),
                            );
                        }
                        if let crate::peer::tunnel::egress::ViaCarry::Carried(carried) = carry {
                            // A carried request is a served request, and it is
                            // recorded exactly like one, the counter, the
                            // wire-session table and the ring buffer, in the order
                            // the terminal outcome below uses. The account and the
                            // credential were ours; only the route out belonged to
                            // a peer, so a carry that did not count would make all
                            // three under-report this fleet's own traffic.
                            // `egress::record_carried` documents which figure it
                            // cannot record and why.
                            //
                            // The headers it folds into the quota are the ORIGIN's
                            // own, carried out of the peer path unfiltered
                            // alongside the client's response, because the client's
                            // copy no longer has them, `is_account_scoped` strips
                            // them there exactly as `build_response` does here.
                            crate::peer::tunnel::egress::record_carried(
                                &manager,
                                crate::peer::tunnel::egress::CarriedRecord {
                                    account_idx: idx,
                                    account: account_name.clone(),
                                    session_key,
                                    session_kind,
                                    wire_session_id: wire_session_id.as_deref(),
                                    model: request_model.clone(),
                                    method: method.to_string(),
                                    path: path_and_query.clone(),
                                    status: carried.response.status().as_u16(),
                                    tool_uses: &wire_tool_uses,
                                    tool_results: &wire_tool_results,
                                    upstream_headers: &carried.upstream_headers,
                                },
                            )
                            .await;
                            break 'attempt Step::Answer(carried.response);
                        }
                    }
                    // `?err`, NOT `%err`. reqwest's `Display` prints only its own top
                    // frame and never walks `source()`, so the h2 sub-kind
                    // (`SendRequest` / `Canceled` / `ChannelClosed`) and its reason code
                    // — the only things that say WHY the connection died — are
                    // structurally unreachable from a `%` rendering. `Debug` carries the
                    // whole chain. Do not "tidy" this back to `%err`.
                    // A CONNECT-phase failure is not a blip on a pooled connection —
                    // there was no connection. Nothing was evicted, nothing is
                    // refreshed by trying again, and the retry is the identical
                    // operation with no new information; all it buys is a second full
                    // `connect_timeout` (10s, `manager/mod.rs`) on a route that is
                    // already not answering. On a blackholed upstream (VPN drop,
                    // captive portal, edge unreachable — no RST, no reply) that
                    // doubles every request's time-to-502 — ~80s to ~160s on an
                    // 8-account fleet — while each request holds its per-account
                    // in-flight slot for the whole of it. Bench the account and let
                    // the rotation do its job.
                    // FIRST arm, ahead of both of the account-level ones: a resolver
                    // failure is a fact about this MACHINE, not about the account that
                    // happened to be selected. Every account resolves the same
                    // hostname, so neither benching this one nor moving to the next can
                    // possibly help — it only unpins the session and burns the fleet.
                    // Hold the pin, wait briefly, retry the SAME account.
                    if is_offline_error(&err) {
                        let offline_failures = ladder.offline_failed();
                        if let Some(offline_wait) =
                            ladder.take_offline_wait(MAX_OFFLINE_WAITS_PER_REQUEST)
                        {
                            tracing::warn!(
                                account_index = idx,
                                account = account_name.as_deref().unwrap_or("?"),
                                offline_wait,
                                max_offline_waits = MAX_OFFLINE_WAITS_PER_REQUEST,
                                error = ?err,
                                "name resolution failed — this machine is offline; \
                                 holding the pinned account and waiting"
                            );
                            tokio::time::sleep(Duration::from_secs(OFFLINE_WAIT_SECS)).await;
                            break 'attempt Step::RetrySame;
                        }
                        break 'attempt Step::Answer(offline_unavailable(
                            provider,
                            offline_failures,
                        ));
                    }
                    // Past the connect phase: a connection existed and the request may
                    // already be in flight on the wire, so this attempt's outcome upstream
                    // is unknown regardless of whether it now gets retried in place or the
                    // ladder rotates away from it. See `unknown_outcome_transport_failure`.
                    if !err.is_connect() {
                        ladder.unknown_outcome();
                    }
                    // Classified ONCE per attempt, see `TransportFailureReason`'s own
                    // doc for why a stream reset needs different handling than a
                    // connection-level death even though both are `!is_connect()`.
                    let transport_reason = classify_transport_failure(&err);
                    if transport_reason == TransportFailureReason::StreamReset
                        && ladder.first_reset_recycle(idx)
                    {
                        // The h2 connection survives a stream reset (unlike a
                        // connection-level death, which reqwest's own error already
                        // evicted from the pool), so without this the retry below
                        // would check out the SAME broken connection. Recycle once per
                        // account per request, a second reset here finds `idx`
                        // already recycled and goes straight to the retry,
                        // which by then really is on the fresh connection.
                        manager.recycle_client_after_reset(idx);
                    }
                    if !err.is_connect()
                        && ladder.transport_retries(idx) < MAX_SAME_ACCOUNT_TRANSPORT_RETRIES
                    {
                        // A blip on this account this request: retry it on a fresh
                        // connection rather than benching an account that is probably fine.
                        // A connection-level death is already evicted from the pool by
                        // the error itself; a stream reset was just recycled above ,
                        // either way each retry genuinely gets a new connection, which
                        // is why repeating is worth more than rotating (94.7% land a
                        // 200).
                        let transport_retry = ladder.spend_transport_retry(idx);
                        // Message text unchanged — `scripts/` parses it. The attempt
                        // fields are ADDED alongside, never a replacement, so an existing
                        // grep keeps matching while a new one can see the ladder.
                        tracing::warn!(
                            account_index = idx,
                            account = account_name.as_deref().unwrap_or("?"),
                            is_connect = err.is_connect(),
                            is_timeout = err.is_timeout(),
                            transport_retry,
                            max_transport_retries = MAX_SAME_ACCOUNT_TRANSPORT_RETRIES,
                            error = ?err,
                            "upstream transport failure — retrying the SAME account on a fresh connection"
                        );
                        break 'attempt Step::RetrySame;
                    }
                    tracing::warn!(
                        account_index = idx,
                        account = account_name.as_deref().unwrap_or("?"),
                        is_connect = err.is_connect(),
                        is_timeout = err.is_timeout(),
                        error = ?err,
                        "upstream transport failure — rotating to another account"
                    );
                    break 'attempt Step::Rotate;
                }
            };
            // This attempt reached an upstream and got an HTTP status back, whatever it
            // was. That fact alone disproves "every account transport-failed".
            ladder.upstream_answered();

            let status = resp.status();
            // One greppable line per upstream response, tagged with the true serving
            // account, the PATH and the outcome status. "serving request" logs BEFORE the
            // outcome, so without this the logs are status-blind — the gap that hid the
            // 401 storm. The path is here because without it a status is undiagnosable:
            // the only other record of which path produced a 404 is the in-memory TUI
            // ring (`push_log` below), which is lost on every restart.
            //
            // INFO only when the answer was not a success. A 2xx is the common case, and
            // a served one is already on its `request usage` line with the same account,
            // path and status; an INFO line for it too would be a second line per request
            // in a log that runs to tens of megabytes a day. The statuses that diagnose
            // something (a 401 storm, a 429, a 404 on an unknown path) stay at INFO.
            if status.is_success() {
                tracing::debug!(
                    account_index = idx,
                    account = account_name.as_deref().unwrap_or("?"),
                    path = %path_and_query,
                    status = status.as_u16(),
                    "upstream response"
                );
            } else {
                tracing::info!(
                    account_index = idx,
                    account = account_name.as_deref().unwrap_or("?"),
                    path = %path_and_query,
                    status = status.as_u16(),
                    "upstream response"
                );
                // The kind a status alone names, once per attempt, for the routing document.
                // Observation only: the arms below keep every retry, hold and rotation.
                if let Some(kind) = crate::observations::ErrorKind::of_status(status.as_u16()) {
                    manager.record_upstream_error(provider, kind);
                }
            }
            let up_headers = resp.headers().clone();
            let rejections = quota_rejections(&up_headers);
            match provider {
                Provider::Codex if status == StatusCode::TOO_MANY_REQUESTS => {}
                Provider::Codex | Provider::Anthropic => {
                    manager.update_quota_serving(&in_flight, &up_headers, status.as_u16());
                }
            }
            // Any non-429 is live proof a rate-limit hold no longer binds.
            if status.as_u16() != 429 {
                manager.clear_rate_limited(idx);
            }

            // 401 → force-refresh once, then retry the SAME account with the fresh
            // token. Neither arm may set the terminal `Error` status — see below.
            if status == StatusCode::UNAUTHORIZED {
                // A 401 on an account with NO refresh token is not churn — there is no
                // refresh that could have rotated the token out from under this
                // request, and none that could ever revive it. The credential is dead
                // and only a re-login cures it, so `Error` (terminal, skipped by
                // selection) is the truthful state. Without this arm a `tcr login
                // --token` account whose token was rejected sat Active forever: never
                // probed (`probeable_indices` wants a refresh token), never refreshed
                // (`refresh_plan` has nothing to send), and rotated away from on every
                // request while `tcr status` reported it healthy.
                if !manager.has_refresh_token(idx) {
                    tracing::error!(
                        account = account_name.as_deref().unwrap_or("?"),
                        account_index = idx,
                        "upstream 401 on an account with no refresh token — credential rejected, re-login needed"
                    );
                    manager.mark_error(idx);
                    break 'attempt Step::Rotate;
                }
                if ladder.first_forced_refresh(idx) {
                    if manager.ensure_fresh_force(idx).await {
                        break 'attempt Step::RetrySame; // the same account, with the fresh token
                    }
                    // The forced refresh produced no new token — it was coalesced /
                    // cooldown-suppressed by the refresh-storm throttle, or the upstream
                    // refresh failed. Retrying would just 401 again on the same dead
                    // token. Rotate away; the account stays Active and the cooldown
                    // self-clears for the next request.
                    break 'attempt Step::Rotate;
                }
                // A SECOND 401: the retry with a freshly-minted token ALSO 401'd. That is
                // almost always ROTATION CHURN — a concurrent request's force-refresh
                // superseded the very token we retried with — and is NOT evidence the
                // credential is dead. So it must never set `Error`.
                //
                // `Error` means "refresh token rejected, re-login needed" and is
                // deliberately TERMINAL: the prober (`probeable_indices`), selection
                // (`Manager::select`) and warming all skip errored rows by design, and the
                // only thing that clears it is a successful refresh — which can never run
                // on a row nobody probes or selects. So condemning a HEALTHY account here
                // is permanent: left unchecked it walks every account to `error` one
                // transient 401 at a time until the proxy goes dark holding a full set of
                // perfectly good tokens (observed live 2026-07-17: 7/7 `error` while every
                // token still probed 200).
                //
                // A genuinely dead credential is still caught at the source: its refresh
                // 400s and the AuthRejected arm sets `Error` there. Here we just rotate —
                // this request fails over and the account stays eligible for the next one.
                break 'attempt Step::Rotate;
            }

            // A JSON auth 403 → upstream refuses the credential WE injected. The client never saw
            // it and can do nothing about it, so this must never reach them as-is:
            // Claude Code reads a 403 as "your own session is dead" and drops the
            // user's login over an account problem that is not theirs. Rotate away, as
            // the 401 branch above does — this account may simply be disabled while its
            // siblings are fine. Codex account access failures rotate without asking for a
            // re-login. Challenges, region blocks and unknown errors pass through without
            // holding healthy accounts: another credential cannot fix a shared IP block.
            //
            // Deliberately does NOT set the terminal `Error` account status, for the
            // same reason the 401 branch above does not: `Error` is permanent, skipped
            // by the prober and by selection, and the only thing that clears it is a
            // successful refresh, which can never run on a row nobody selects again.
            // A 403 here is evidence about THIS request's account access, not proof the
            // refresh token itself is dead — that is still caught at the source, where
            // a rejected refresh sets `Error` directly.
            //
            // It does arm a SHORT hold (`Manager::mark_refused`), which self-frees and
            // keeps the status `Active`. Without one, rotation brought the account
            // straight back and the next request paid the same refused round trip,
            // while `tcr status` read the row as `gate: ok`; the hold's gate reads
            // `refused` instead.
            // Refusal diagnostics use response-header allowlists only, never a whole map or
            // request credentials, cookies or body content.
            let header_str = |name: &str| {
                up_headers
                    .get(name)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("<absent>")
            };
            if status == StatusCode::FORBIDDEN {
                tracing::info!(
                    account = account_name.as_deref().unwrap_or("?"),
                    account_index = idx,
                    provider = provider.as_str(),
                    content_type = header_str("content-type"),
                    cf_mitigated = header_str("cf-mitigated"),
                    cf_ray = header_str("cf-ray"),
                    server = header_str("server"),
                    request_id = header_str("request-id"),
                    x_request_id = header_str("x-request-id"),
                    retry_after_raw = header_str("retry-after"),
                    "403 diagnostic"
                );
                // Upstream's own words for the refusal, read capped in size and time
                // exactly as the 429 arm reads its body, so a body that never
                // completes cannot become a hang. A Codex body can also be forwarded, so it gets
                // the ordinary response cap rather than the smaller diagnostic-only cap.
                let body_cap = match provider {
                    Provider::Anthropic => RATE_LIMIT_BODY_CAP,
                    Provider::Codex => MAX_BODY_BYTES,
                };
                let body = if let Ok(Ok(body)) = tokio::time::timeout(
                    Duration::from_secs(RATE_LIMIT_BODY_READ_SECS),
                    read_capped_body(resp.bytes_stream(), body_cap),
                )
                .await
                {
                    log_upstream_error_body(idx, account_name.as_deref(), status, &body);
                    Some(body)
                } else {
                    tracing::warn!(
                        account = account_name.as_deref().unwrap_or("?"),
                        account_index = idx,
                        "upstream 403 body could not be read within the cap"
                    );
                    None
                };
                match provider {
                    Provider::Codex => {
                        if body.is_none() {
                            let hold = NO_GUIDANCE_HOLD_SECS
                                + i64::from(now.nanosecond()) % (NO_GUIDANCE_JITTER_MAX_SECS + 1);
                            manager.mark_rate_limited(idx, hold);
                            ladder.codex_access_unread();
                            break 'attempt Step::Rotate;
                        }
                        let auth_error = body
                            .as_deref()
                            .is_some_and(|body| codex_forbidden_is_auth_error(&up_headers, body));
                        if !auth_error {
                            if body.as_deref().is_some_and(|body| {
                                codex_forbidden_is_account_error(&up_headers, body)
                            }) {
                                manager.mark_refused(idx);
                                ladder.account_access_blocked(body.map(|body| (up_headers, body)));
                                break 'attempt Step::Rotate;
                            }
                            tracing::warn!(
                                account_index = idx,
                                "upstream Codex request blocked without an account failure; forwarding unchanged"
                            );
                            break 'attempt Step::Answer(match body {
                                Some(body) => build_response(
                                    status,
                                    &up_headers,
                                    Body::from(body),
                                    ServedBy::PooledAccount,
                                ),
                                None => ClientError::new(
                                    StatusCode::BAD_GATEWAY,
                                    ErrorKind::Proxy,
                                    "Failed to read the upstream access-denied response.",
                                )
                                .respond(provider),
                            });
                        }
                    }
                    Provider::Anthropic => {}
                }
                tracing::warn!(
                    account = account_name.as_deref().unwrap_or("?"),
                    account_index = idx,
                    "upstream 403 Forbidden — credential refused, rotating to another account"
                );
                manager.mark_refused(idx);
                ladder.refused_403(idx, account_name.clone());
                break 'attempt Step::Rotate;
            }

            // 429 → durable quota rejection rotates; a transient limit waits/rotates.
            if status == StatusCode::TOO_MANY_REQUESTS {
                tracing::info!(
                    account = account_name.as_deref().unwrap_or("?"),
                    account_index = idx,
                    retry_after_raw = header_str("retry-after"),
                    retry_after_parsed = format!("{:?}", parse_retry_after(&up_headers)),
                    unified_status = header_str("anthropic-ratelimit-unified-status"),
                    // The MODEL-SCOPED twin of `unified_status`. Logged to make one
                    // specific class observable: a Fable-weekly-only rejection, which
                    // upstream also reports in the aggregate `unified_status`. Reading
                    // the aggregate alone benched the whole account and re-keyed the
                    // session (issue #178); `quota_rejections` now separates the two,
                    // and this line is how a live response is checked against it —
                    // diagnostic only, no routing reads it.
                    unified_7d_oi_status = header_str("anthropic-ratelimit-unified-7d_oi-status"),
                    unified_5h_reset = header_str("anthropic-ratelimit-unified-5h-reset"),
                    unified_7d_reset = header_str("anthropic-ratelimit-unified-7d-reset"),
                    quota_rejected = !rejections.is_empty(),
                    provider = provider.as_str(),
                    codex_primary_used_percent = header_str("x-codex-primary-used-percent"),
                    codex_primary_window_minutes = header_str("x-codex-primary-window-minutes"),
                    codex_primary_reset_at = header_str("x-codex-primary-reset-at"),
                    codex_secondary_used_percent = header_str("x-codex-secondary-used-percent"),
                    codex_secondary_window_minutes = header_str("x-codex-secondary-window-minutes"),
                    codex_secondary_reset_at = header_str("x-codex-secondary-reset-at"),
                    codex_rate_limit_reached_type = header_str("x-codex-rate-limit-reached-type"),
                    codex_credits_has_credits = header_str("x-codex-credits-has-credits"),
                    codex_credits_unlimited = header_str("x-codex-credits-unlimited"),
                    codex_credits_balance = header_str("x-codex-credits-balance"),
                    content_type = header_str("content-type"),
                    cf_mitigated = header_str("cf-mitigated"),
                    cf_ray = header_str("cf-ray"),
                    server = header_str("server"),
                    request_id = header_str("request-id"),
                    x_request_id = header_str("x-request-id"),
                    // WHO asked, so a 429 that follows one client around can be told
                    // from one that follows an account. Measured 2026-09-24: 143 of 145
                    // requests to `/v1/messages` WITHOUT `?beta=true` got a 429 with no
                    // quota headers, on accounts at 0% used, while 10,575 of 10,575 on
                    // `?beta=true` got a 200, and nothing in the log named the client.
                    user_agent = req_headers
                        .get(USER_AGENT)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("<absent>"),
                    model = request_model.as_deref().unwrap_or("?"),
                    "429 diagnostic"
                );
                let retry_after_raw = parse_retry_after(&up_headers);
                // A 429 about the REQUEST, not the account: no quota guidance at all, on
                // a request that did not come through Claude Code's own client. Parking
                // the account for it is wrong and rotating is wasted, so it is forwarded
                // as-is and every account stays in rotation. See
                // [`is_request_scoped_429`] for the measurements that draw this line.
                // Those measurements are Anthropic's (`?beta=true`, `unified-status`), and no
                // Codex request or 429 carries either, so a Codex 429 never takes this branch:
                // it would forward every one with no account held. Codex's arm is below.
                match provider {
                    Provider::Anthropic
                        if is_request_scoped_429(
                            &path_and_query,
                            &up_headers,
                            !rejections.is_empty(),
                        ) =>
                    {
                        tracing::warn!(
                            account = account_name.as_deref().unwrap_or("?"),
                            account_index = idx,
                            path = %path_and_query,
                            "upstream 429 with no quota guidance on a request without ?beta=true: \
                             refused for the request, not the account, so it is forwarded and no \
                             account is parked"
                        );
                        // Falls through to the terminal outcome below, which counts the
                        // request, logs the body (`log_upstream_error_body`) and forwards it
                        // verbatim, the same path a 400 or a spent 529 takes.
                    }
                    Provider::Anthropic | Provider::Codex => {
                        // Read the bounded body before deciding whether Codex refused quota. Some
                        // quota errors carry no reached-type header at all.
                        let refusal_body = if let Ok(Ok(body)) = tokio::time::timeout(
                            Duration::from_secs(RATE_LIMIT_BODY_READ_SECS),
                            read_capped_body(resp.bytes_stream(), RATE_LIMIT_BODY_CAP),
                        )
                        .await
                        {
                            log_upstream_error_body(idx, account_name.as_deref(), status, &body);
                            body
                        } else {
                            tracing::warn!(
                                account_index = idx,
                                "upstream 429 body could not be read within the cap"
                            );
                            Bytes::new()
                        };
                        let retry_after = retry_after_raw.unwrap_or(60);
                        // Codex quota bodies name the reset in Unix seconds; prefer it to header
                        // windows, then retry-after, then the short default. Plain rate limits keep
                        // their retry-after even when a response also carries window readings.
                        match provider {
                            Provider::Codex => {
                                let clock = OffsetDateTime::now_utc();
                                let refusal = crate::quota::CodexQuotaRefusal::parse(
                                    &up_headers,
                                    &refusal_body,
                                    clock,
                                );
                                manager.update_quota_with_codex_refusal_serving(
                                    &in_flight,
                                    &up_headers,
                                    429,
                                    refusal.as_ref(),
                                );
                                let quota_refusal = refusal.is_some();
                                let hold = refusal
                                    .as_ref()
                                    .and_then(|refusal| refusal.reset)
                                    .map_or(retry_after, |reset| {
                                        reset
                                            .unix_timestamp()
                                            .saturating_sub(clock.unix_timestamp())
                                    });
                                // A quota refusal is held and benched; a plain rate limit parks
                                // on its timer, exactly as the Anthropic arm below parks one.
                                let step = if quota_refusal {
                                    manager.mark_codex_quota_limited(idx, hold);
                                    Step::Rotate
                                } else {
                                    let count = ladder.retries_429(idx);
                                    let jitter = i64::from(clock.nanosecond())
                                        % (NO_GUIDANCE_JITTER_MAX_SECS + 1);
                                    match classify_transient_429(retry_after_raw, count, jitter) {
                                        Transient429::InlineWait(wait) => {
                                            ladder.spend_429_retry(idx);
                                            tokio::time::sleep(Duration::from_secs(wait as u64))
                                                .await;
                                            break 'attempt Step::RetrySame;
                                        }
                                        Transient429::Park(wait) => {
                                            manager.mark_rate_limited(idx, wait);
                                            Step::Park
                                        }
                                    }
                                };
                                tracing::info!(
                                    account_index = idx,
                                    quota_refusal,
                                    "Codex 429: holding the account and rotating"
                                );
                                break 'attempt step;
                            }
                            Provider::Anthropic => {}
                        }
                        if !rejections.is_empty() {
                            // A model-scoped rejection is recorded in the window the selector
                            // already gates that model class on, NOT as an account-wide hold:
                            // the account still serves every other model class, and the window
                            // self-frees at its own reset (issue #178). A rejection any shared
                            // scope reports is account-wide and still arms the hold — clamped
                            // to `MAX_RATE_LIMIT_HOLD_SECONDS` by `mark_rate_limited`, whose
                            // doc-comment explains why durable exhaustion must be carried by
                            // the learned quota rather than by an unbounded hold.
                            if rejections.contains(&UnifiedRejectionKind::FableWeekly) {
                                manager.mark_model_weekly_rejected(idx, &up_headers);
                            }
                            if rejections
                                .iter()
                                .any(|kind| !matches!(kind, UnifiedRejectionKind::FableWeekly))
                            {
                                manager.mark_rate_limited(
                                    idx,
                                    jittered_quota_hold(retry_after, now.nanosecond()),
                                );
                            }
                            break 'attempt Step::Rotate;
                        }
                        let count = ladder.retries_429(idx);
                        // Desync the no-guidance un-park across accounts that tripped together;
                        // derive the jitter from the loop clock so no `rand` dependency is needed.
                        let jitter =
                            i64::from(now.nanosecond()) % (NO_GUIDANCE_JITTER_MAX_SECS + 1);
                        match classify_transient_429(retry_after_raw, count, jitter) {
                            Transient429::InlineWait(wait) => {
                                ladder.spend_429_retry(idx);
                                tokio::time::sleep(Duration::from_secs(wait as u64)).await;
                                break 'attempt Step::RetrySame; // after the bounded wait
                            }
                            Transient429::Park(wait) => {
                                manager.mark_rate_limited(idx, wait);
                                // Benched AND re-admittable: a timer that expires on its own, so
                                // the soft-wait above may take it back. The Codex arm above parks
                                // its non-quota hold the same way; every other bench in this loop
                                // is `Step::Rotate`, which is not re-admitted.
                                break 'attempt Step::Park;
                            }
                        }
                    }
                }
            }

            // 529 Overloaded → this SEND was refused, and the ladder below spends two
            // budgets before the client ever sees it. Measured live 2026-07-26: 256 of
            // the last 4,963 upstream responses were 529 (5.2%), every one of them
            // surfaced in Claude Code as a failed request the user re-sent by hand.
            //
            // FIRST the SAME account, after a short escalating backoff: its prompt cache
            // is warm and a brief overload usually clears within a second or two.
            //
            // THEN — and this is what the arm originally refused to do — a bounded
            // FAILOVER to a different account. The original premise was that a 529 means
            // the SERVER is saturated, so a sibling would be equally overloaded and the
            // rotation would buy a cold prompt cache for nothing. The live log falsifies
            // it: the overload is ACCOUNT-scoped and time-varying (one account answered
            // 136 529s over 8 minutes while its siblings served 200s; a session diverted
            // off it got a 200 from a sibling 2s later). See
            // [`MAX_529_FAILOVERS_PER_REQUEST`].
            //
            // The failover mechanism is benching `idx` in `tried` (`Ladder::fail_over_529`), because
            // `select` already has the branch for it: a PINNED session whose pin is in
            // `tried` takes the documented "pin-tried" path, which diverts THIS REQUEST
            // and KEEPS THE PIN (`src/manager/select.rs`). So the session comes home to
            // its warm account on its next request; nothing re-keys, and `tried` is
            // per-request so the 529'd account is fully eligible again immediately.
            // `mark_rate_limited` is still NEVER called here — an overloaded account is
            // not over quota, and arming a hold would bench it for OTHER requests.
            //
            // The failover is gated on `select` actually offering another account, and
            // that gate is load-bearing rather than defensive: benching the account
            // FIRST and discovering the fleet is empty AFTERWARDS would drop this
            // request into the exhausted path and answer a synthesized 429 — replacing
            // the upstream's own 529 with a status that means something else entirely.
            // With no account to move to, the arm degrades exactly into its pre-failover
            // self. (Under a hard account lock `select` returns `None` for a tried
            // account by construction, so the lock's "no failover to the pool" contract
            // holds here for free.)
            //
            // Latency: the in-flight guard taken at the top of this iteration is HELD
            // across every backoff, so a retrying request keeps its per-account
            // concurrency slot. Per account that is MAX_SAME_ACCOUNT_529_RETRIES waits
            // each clamped to RETRY_529_MAX_BACKOFF_SECS (3s on the no-`retry-after`
            // ladder the live captures actually carry, 8s if the upstream asks for
            // more); per REQUEST it is that times `1 + MAX_529_FAILOVERS_PER_REQUEST`,
            // i.e. 9s and 24s. Both are asserted, not assumed —
            // `overloaded_529_failover_worst_case_latency_is_bounded`.
            //
            // On BOTH budgets being spent this does NOT return. Control falls through to
            // the terminal-outcome arm below, so the client sees the 529 forwarded
            // verbatim — byte-identical to the behaviour before this arm existed.
            if status.as_u16() == STATUS_OVERLOADED {
                let retried = ladder.retries_529(idx);
                if retried < MAX_SAME_ACCOUNT_529_RETRIES {
                    let backoff = backoff_529_secs(retried, parse_retry_after(&up_headers));
                    let retry = ladder.spend_529_retry(idx);
                    tracing::warn!(
                        account = account_name.as_deref().unwrap_or("?"),
                        account_index = idx,
                        retry,
                        max_retries = MAX_SAME_ACCOUNT_529_RETRIES,
                        backoff_secs = backoff,
                        "upstream 529 Overloaded — backing off and retrying the SAME account"
                    );
                    tokio::time::sleep(Duration::from_secs(backoff)).await;
                    break 'attempt Step::RetrySame; // after the bounded backoff
                }
                // This account's in-place budget is spent. Fail the request over while
                // the per-request failover budget lasts AND another account is free.
                let attempts = retried + 1;
                if ladder.failovers_529() < MAX_529_FAILOVERS_PER_REQUEST {
                    // Ask for the replacement BEFORE benching this account, and pass the
                    // `tried` set the next iteration WOULD have passed. A fresh clock,
                    // not the loop's `now`: the backoffs above have made that value up to
                    // several seconds stale, and an account whose hold expired during
                    // them is a legitimate target.
                    let failover_now = OffsetDateTime::now_utc();
                    if let Some(other) = ladder.probe_without(idx, |with_this_one| {
                        manager.select_with_group(
                            with_this_one,
                            failover_now,
                            request_model.as_deref(),
                            session_key,
                            &path,
                            conn_key,
                            request_group.as_deref(),
                        )
                    }) {
                        debug_assert_ne!(
                            other, idx,
                            "select must honour `tried`, or the failover re-sends to the overloaded account"
                        );
                        // The probe above IS this failover's selection; `Step::FailOver`
                        // consumes it rather than re-selecting (see `Ladder`'s `next_idx`).
                        tracing::warn!(
                            account = account_name.as_deref().unwrap_or("?"),
                            account_index = idx,
                            attempts,
                            // The failover this step spends, counted before
                            // `Ladder::fail_over_529` applies it.
                            failover = ladder.failovers_529() + 1,
                            max_failovers = MAX_529_FAILOVERS_PER_REQUEST,
                            next_account = manager.account_name(other).as_deref().unwrap_or("?"),
                            next_account_index = other,
                            "upstream 529 Overloaded — in-place budget spent, failing this request over to another account"
                        );
                        break 'attempt Step::FailOver(other);
                    }
                }
                tracing::warn!(
                    account = account_name.as_deref().unwrap_or("?"),
                    account_index = idx,
                    attempts,
                    // Distinguishes the two give-up shapes in the log: 0 means nothing
                    // else was eligible to fail over to, `MAX_529_FAILOVERS_PER_REQUEST`
                    // means the fleet was walked and every account was overloaded too.
                    failovers = ladder.failovers_529(),
                    "upstream 529 Overloaded — retry and failover budgets spent, forwarding 529 to the client"
                );
            }

            Step::Forward {
                resp,
                status,
                up_headers,
            }
        };
        // The one place the ladder moves for an attempt.
        let (resp, status, up_headers) = match ladder.apply(idx, step) {
            ControlFlow::Continue(None) => continue,
            ControlFlow::Break(response) => return response,
            ControlFlow::Continue(Some(forward)) => forward,
        };

        // Terminal outcome (2xx, or a 3xx/4xx/5xx forwarded verbatim). Count the
        // request once against the true serving account and log it.
        // F1: the wire-session table (`tcr status --json`'s `sessions` array) gets this
        // request's session id, model and tool events. Cloned, not moved: the usage-recording
        // arms below (streamed and non-streamed) still need `account_name`/`request_model`.
        record_served_request(
            &manager,
            ServedRequest {
                idx,
                served_at: OffsetDateTime::now_utc(),
                session_key,
                session_kind,
                wire_session_id: wire_session_id.as_deref(),
                account: account_name.clone(),
                method: method.to_string(),
                path_and_query: path_and_query.clone(),
                status: status.as_u16(),
                // The provider this request was routed on, decided once from its path.
                wire: match provider {
                    Provider::Codex => WireFold::Codex { path: &path },
                    Provider::Anthropic => WireFold::Anthropic {
                        model: request_model.clone(),
                        tool_uses: &wire_tool_uses,
                        tool_results: &wire_tool_results,
                    },
                },
            },
        );
        // A history bound elsewhere and answered here has moved here: re-bind its tokens, or the
        // next full-history request is held back to the old account's cold cache once it
        // recovers (`Manager::rebind_served_history`).
        if status.is_success() && minted.is_some_and(|(bound_idx, _)| bound_idx != idx) {
            manager.rebind_served_history(
                &bound.tokens,
                idx,
                request_model.as_deref(),
                crate::now_ms(),
            );
        }

        let declared_stream = up_headers
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.contains("text/event-stream"));
        // The header is not the whole answer: a 2xx SSE body under any other Content-Type was
        // buffered and parsed as JSON, so its tokens never reached the ledger. Such a body's
        // first bytes decide, and the chunks read to decide go back in front of the rest.
        let mut upstream_body = resp.bytes_stream();
        let (is_stream, sniffed) = if declared_stream || !status.is_success() {
            (declared_stream, Vec::new())
        } else {
            sniff_sse(&mut upstream_body).await
        };
        if is_stream && !declared_stream {
            tracing::debug!(
                account_index = idx,
                content_type = ?up_headers.get(CONTENT_TYPE),
                "a 2xx body not labelled text/event-stream opens as SSE; reading it as a stream"
            );
        }
        let upstream_body = futures::stream::iter(sniffed).chain(upstream_body);

        if is_stream {
            // Tee: the passthrough to the client is NEVER gated on the parser.
            // Each chunk is offered to a BOUNDED side channel (`SSE_TEE_CAPACITY`)
            // that a spawned task drains through eventsource-stream (which
            // reassembles events split across chunks). The bound stops a fast
            // upstream + slow parser from retaining the whole response in the
            // channel; `try_send` on the passthrough side keeps that bound from
            // ever applying backpressure to the client — see below.
            let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(SSE_TEE_CAPACITY);
            // Distinguishes WHY the tee ends. A plain `.map()` over the upstream
            // stream (this passthrough's previous shape) only ever sees `Some`
            // items — it cannot observe `Ready(None)`/the stream being dropped,
            // so a client hitting Esc mid-turn (axum drops the response body,
            // dropping everything the passthrough owns) looked IDENTICAL, from
            // inside the parser, to the upstream finishing the stream itself.
            // This oneshot is consumed exactly once, the moment `TeeState` (below)
            // observes the WRAPPED stream reach a genuine terminal state — clean
            // EOF or a transport error, both real signals ABOUT THE UPSTREAM. If
            // `TeeState` is dropped before either happens (client walked away),
            // the sender drops unfired and `ended_rx` resolves to `Err` — the
            // signal for "we never learned how the stream would have ended,"
            // which must never be booked as truncation.
            let (ended_tx, ended_rx) = tokio::sync::oneshot::channel::<()>();
            // Set the instant a teed chunk is dropped by the bounded channel
            // below (full channel or a dead receiver). A dropped chunk might
            // have been the one carrying `message_stop` — from that point on,
            // "no message_stop was observed" no longer means "the turn was
            // truncated", it means "we don't know", so the classifier abstains
            // instead of guessing.
            let evidence_dropped = Arc::new(AtomicBool::new(false));
            let evidence_dropped_reader = Arc::clone(&evidence_dropped);
            let manager_side = Arc::clone(&manager);
            let status_is_success = status.is_success();
            // Cloned, not moved: the spawned parser task below outlives this iteration, and
            // `account_name` (the original) is still moved into `TeeState` further below, in
            // this same iteration (see the comment there). `extended_ttl` is the answer, a
            // `bool`: the spawned task never holds the body.
            let turn = ServedTurn {
                provider,
                idx,
                account_name: account_name.clone(),
                path: path.clone(),
                session_key,
                wire_session_id: wire_session_id.clone(),
                request_model: request_model.clone(),
                extended_ttl,
            };
            // The request's clock moves into the passthrough body (`StreamTiming`),
            // which finishes it when the body ends and sends the fields here, to
            // the task that owns the `request usage` line.
            let (timing_tx, timing_rx) = tokio::sync::oneshot::channel::<TimingFields>();
            let stream_timing = StreamTiming {
                timing: timing.hand_off_stream(),
                status: status.as_u16(),
                done: Some(timing_tx),
            };
            tokio::spawn(async move {
                let byte_stream = futures::stream::unfold(rx, |mut rx| async move {
                    rx.recv()
                        .await
                        .map(|bytes| (Ok::<Bytes, Infallible>(bytes), rx))
                });
                let SseParse {
                    usage: parsed,
                    model: usage_model,
                    response_model,
                    stream_error,
                    tool_uses: response_tool_uses,
                    bound_tokens,
                    codex_tool_calls,
                    codex_stream_limit,
                } = parse_sse_usage(byte_stream, provider, &turn.path).await;
                // A 200 can carry a quota refusal in its SSE body. The client has
                // already received that stream, so never replay it here; bench the
                // serving account before its next pinned turn is selected.
                if let Some(limit) = codex_stream_limit {
                    manager_side.update_quota_with_codex_refusal(
                        idx,
                        &limit.headers,
                        429,
                        limit.refusal.as_ref(),
                    );
                    if limit.refusal.is_some() {
                        manager_side.mark_codex_quota_limited(idx, limit.hold_seconds);
                    } else {
                        manager_side.mark_rate_limited(idx, limit.hold_seconds);
                    }
                }
                // Who minted the account-bound tokens in this turn. Recorded before anything
                // else the parse learned, because it is the only one of them that changes how a
                // LATER request is routed: every following turn of this conversation echoes
                // these back, and any account but `idx` rejects the whole request.
                manager_side.record_bound_tokens(&bound_tokens, idx, crate::now_ms());
                // RUNNING NOW: every tool this turn asked for starts HERE — the stream has
                // just ended, so the client is beginning them as this line runs. Outside the
                // usage guard below on purpose: a turn that emitted tool calls but reported
                // no usage still has tools running.
                let stream_ended = OffsetDateTime::now_utc();
                manager_side.record_wire_session_tool_uses(
                    turn.wire_session_id.as_deref(),
                    stream_ended,
                    &response_tool_uses,
                );
                // The Codex twin: the calls this turn's finished output items asked for,
                // running from the same instant with their own timeouts. Empty on an
                // Anthropic stream.
                manager_side.record_codex_wire_session_calls(
                    turn.wire_session_id.as_deref(),
                    stream_ended,
                    &codex_tool_calls,
                );
                // The body's end. The parse above runs until the tee's sender
                // drops, which is the same drop that sends this, so it is already
                // here or about to be. A parse that stopped early on a malformed
                // frame waits here for the stream to finish, which is when
                // `total_ms` is known.
                let timing = if let Ok(fields) = timing_rx.await {
                    Some(fields)
                } else {
                    tracing::warn!(
                        account_index = idx,
                        "streamed request's timing was dropped unsent; \
                         its request usage line carries no timing"
                    );
                    None
                };
                if status.is_success() {
                    manager_side.record_response_model(provider, response_model.as_deref());
                }
                if parsed.input_total > 0 || parsed.output > 0 {
                    turn.finish(&manager_side, status, &parsed, usage_model, timing.as_ref());
                } else if let Some(t) = &timing {
                    // No usage, so no `request usage` line to ride on.
                    t.log_alone();
                }
                // Sibling of the usage guard above, not nested in it: an error
                // event with NO message_start leaves input_total == output == 0,
                // and this must still be recorded — that is precisely the bug
                // (a truncated stream booked as a clean serve). Called ONCE per
                // stream, after the parse loop ends, never per event — a
                // per-event call on a long erroring stream is a log-flood vector.
                //
                // A forwarded 3xx/4xx/5xx SSE body never had a `message_stop`
                // contract to begin with — it is an error response, not a
                // truncated turn — so the SYNTHESIZED verdict below only fires
                // once the upstream actually answered 2xx. That status gate
                // must scope to the synthesized branch ALONE: a genuine in-band
                // `error` event is a positive observation read directly off
                // the wire, independent of the response's status line, and
                // wrapping it in the same `if` too was a coverage regression —
                // a real error on a forwarded non-2xx body went unrecorded.
                if let Some(kind) = stream_error {
                    if kind == TRUNCATED_STREAM_ERROR_KIND {
                        // The synthesized verdict is an ABSENCE (no
                        // `message_stop` seen) rather than a positive
                        // observation, and an absence is only evidence when we
                        // know we saw everything there was to see — hence the
                        // status gate plus the ended/evidence checks below,
                        // which the positive case in the `else` skips entirely.
                        if status_is_success {
                            let ended_naturally = ended_rx.await.is_ok();
                            let dropped = evidence_dropped_reader.load(Ordering::SeqCst);
                            if ended_naturally && !dropped {
                                manager_side.record_stream_error(idx, &kind);
                            } else {
                                tracing::debug!(
                                    account_index = idx,
                                    ended_naturally,
                                    evidence_dropped = dropped,
                                    "SSE stream ended without message_stop but the tee \
                                     cannot prove why — abstaining rather than recording \
                                     a fabricated truncation"
                                );
                            }
                        }
                    } else {
                        // An in-band `error` event is a POSITIVE observation —
                        // we directly read those bytes off the wire, so it is
                        // recorded regardless of the response status and
                        // regardless of how the tee itself ended.
                        manager_side.record_stream_error(idx, &kind);
                    }
                }
            });

            // Threads the tee's mutable state — the wrapped byte stream, the
            // side-channel sender, the dropped-evidence flag, the `ended`
            // oneshot, and the `_in_flight` RAII guard — through
            // `stream::unfold` so all of it lives exactly as long as the
            // passthrough stream does and drops in ONE place, together. That
            // single drop site is what lets `ended` (this arm's job: tell the
            // parser task client-abandonment from upstream-completion) and
            // `_in_flight` (unrelated: pacing's load counter) each fire exactly
            // once, at real completion — clean EOF, transport error, OR client
            // disconnect — rather than at response-headers. See the long-form
            // comment this replaced for why `_in_flight` cannot just be a
            // handler-local.
            // Generic over the concrete upstream stream type rather than boxed
            // as `dyn Stream` — there is exactly one call site (below), so a
            // trait object bought nothing but a v-table indirection and a heap
            // allocation on every streamed response.
            struct TeeState<S> {
                inner: S,
                tx: tokio::sync::mpsc::Sender<Bytes>,
                evidence_dropped: Arc<AtomicBool>,
                ended: Option<tokio::sync::oneshot::Sender<()>>,
                account_index: usize,
                account_name: Option<String>,
                _in_flight: InFlightGuard,
                timing: StreamTiming,
            }
            let state = TeeState {
                inner: upstream_body,
                tx,
                evidence_dropped,
                ended: Some(ended_tx),
                account_index: idx,
                // Moved, not cloned: this is the last use of `account_name` in
                // this iteration — the retry loop above already re-fetches its
                // own copy from `manager.account_name(idx)` on the next pass.
                account_name,
                _in_flight: in_flight,
                timing: stream_timing,
            };
            let passthrough = futures::stream::unfold(state, |mut state| async move {
                match state.inner.next().await {
                    Some(Ok(bytes)) => {
                        // `first_byte_ms`: taken where the chunk is handed on
                        // toward the client, before the tee offers it to the
                        // parser. One `Option` check per chunk, set once.
                        state.timing.note_first_byte();
                        // Best-effort tee: `try_send` never blocks the
                        // passthrough. A FULL channel (slow/starved parser)
                        // drops the chunk for the PARSER only — usage counting
                        // becomes best-effort under backpressure; the client
                        // stream forwards untouched. That drop is genuine
                        // evidence loss and flips `evidence_dropped` so the
                        // parser task can tell "no message_stop" apart from
                        // "no message_stop THAT WE COULD SEE". A CLOSED
                        // receiver is a different thing entirely — the parser
                        // task's own `rx`-side stream already ended and it does
                        // not want any more bytes — so it must not poison the
                        // verdict the same way; see [`is_genuine_evidence_loss`].
                        if let Err(err) = state.tx.try_send(bytes.clone())
                            && is_genuine_evidence_loss(&err)
                        {
                            state.evidence_dropped.store(true, Ordering::SeqCst);
                        }
                        Some((Ok(bytes), state))
                    }
                    Some(Err(err)) => {
                        // The transport died AFTER headers were already
                        // forwarded — this `Err` is what silently reached the
                        // client as a severed connection while the request was
                        // already booked as a clean 200 upstream response.
                        // Nothing else observes a body-level transport
                        // failure, so this is the only place it is ever
                        // logged. Same field shape as the pre-header
                        // transport-failure ladder above, so both halves of a
                        // request's lifecycle log alike. It is ALSO a genuine
                        // observation about the upstream, not a client
                        // abandonment — fire `ended` before returning it.
                        tracing::warn!(
                            account_index = state.account_index,
                            account = state.account_name.as_deref().unwrap_or("?"),
                            error = ?err,
                            "upstream transport failure mid-stream — forwarding the \
                             severed connection to the client"
                        );
                        if let Some(ended) = state.ended.take()
                            && ended.send(()).is_err()
                        {
                            tracing::debug!("stream-end receiver already dropped");
                        }
                        state.timing.mark_ended();
                        Some((Err(err), state))
                    }
                    None => {
                        // Clean EOF — the upstream itself closed the stream.
                        // Also a genuine observation. Client abandonment never
                        // reaches this arm at all: axum drops the response
                        // body (hence this whole stream, mid-poll) instead of
                        // ever seeing it reach `None`, which drops `state` —
                        // and `state.ended` along with it — unfired.
                        if let Some(ended) = state.ended.take()
                            && ended.send(()).is_err()
                        {
                            tracing::debug!("stream-end receiver already dropped");
                        }
                        // `state` drops as this arm returns, and with it
                        // `StreamTiming`, which reads this as the end.
                        state.timing.mark_ended();
                        None
                    }
                }
            });
            return build_response(
                status,
                &up_headers,
                Body::from_stream(passthrough),
                ServedBy::PooledAccount,
            );
        }

        // Non-stream body: buffer it (capped like the request path — an unbounded
        // `resp.bytes().await` is the same DoS surface `MAX_BODY_BYTES` guards on
        // the way in), parse usage from the JSON, forward it.
        let bytes = match read_capped_body(upstream_body, MAX_BODY_BYTES).await {
            Ok(bytes) => bytes,
            Err(BodyReadError::Transport) => {
                manager.record_upstream_error(provider, crate::observations::ErrorKind::Transport);
                return ClientError::new(
                    StatusCode::BAD_GATEWAY,
                    ErrorKind::Proxy,
                    "Failed to read upstream response body.",
                )
                .respond(provider);
            }
            Err(BodyReadError::TooLarge) => {
                return ClientError::new(
                    StatusCode::BAD_GATEWAY,
                    ErrorKind::Proxy,
                    "Upstream response body exceeded the size cap.",
                )
                .respond(provider);
            }
        };
        timing.mark_body_buffered();
        match provider {
            Provider::Codex => {
                if status == StatusCode::BAD_REQUEST
                    && let Some(detail) = codex_model_refusal_detail(&bytes)
                {
                    log_upstream_error_body(idx, account_name.as_deref(), status, &bytes);
                    // Capacity: this account cannot serve the model now, which is not overload.
                    manager
                        .record_upstream_error(provider, crate::observations::ErrorKind::Capacity);
                    ladder.model_refused(account_name.clone(), detail);
                    ladder.bench(idx);
                    continue;
                }
            }
            Provider::Anthropic => {}
        }
        // Only a forwarded body is the client's first byte. A model rejection
        // consumed here must not timestamp a later sibling's successful stream.
        timing.forward_buffered_body();
        if status.is_success() {
            // One parse of the body for all three readings below.
            let response = parse_non_streamed_response(&bytes, provider);
            // RUNNING NOW, non-streamed twin of the SSE arm above: the body is complete, so
            // any `tool_use` in it starts now. Outside the usage guard for the same reason.
            manager.record_wire_session_tool_uses(
                wire_session_id.as_deref(),
                OffsetDateTime::now_utc(),
                &response.tool_uses,
            );
            // Non-streamed twin of the mint record on the SSE arm above.
            manager.record_bound_tokens(&response.bound_tokens, idx, crate::now_ms());
            let parsed = response.usage;
            let response_model = response.model;
            manager.record_response_model(provider, response_model.as_deref());
            if parsed.input_total > 0 || parsed.output > 0 {
                // The `request usage` line carries the timing, so `handle` must not log it a
                // second time: disarmed. No Codex tool calls are read here: codex streams its
                // turns, so a non-streamed Responses object comes only from a client that is
                // not codex.
                let t = timing.finish_for_usage_line(status.as_u16(), Instant::now());
                ServedTurn {
                    provider,
                    idx,
                    account_name: account_name.clone(),
                    path: path.clone(),
                    session_key,
                    wire_session_id: wire_session_id.clone(),
                    request_model: request_model.clone(),
                    extended_ttl,
                }
                .finish(&manager, status, &parsed, response_model, Some(&t));
            }
        } else {
            // The "upstream response" line above (:2587) has never carried a
            // body: it fires before this buffer exists, and for a streamed 2xx
            // there never is one to log. `bytes` is already fully buffered by
            // the time we get here regardless of status — logging from it adds
            // no extra body read on the response path, only diagnosis for what
            // was previously an opaque status code (64 `status=400` lines with
            // no error type or message, measured 2026-09-07).
            log_upstream_error_body(idx, account_name.as_deref(), status, &bytes);
        }
        return build_response(
            status,
            &up_headers,
            Body::from(bytes),
            ServedBy::PooledAccount,
        );
    }

    // Ran out of attempts while still rotating: the same verdict the select-found-nothing
    // arm reads off the tallies, and otherwise an upstream did answer us and the honest
    // verdict is exhausted quota.
    let now = OffsetDateTime::now_utc();
    // The MIRROR of the refusal arm above, and the same fix: a strict ask is
    // `reserved || !spill`, so this terminal's own scoping has to ask the same
    // question the picker did.
    let strict_group = request_group
        .as_deref()
        .filter(|g| manager.is_group_strict(g));
    if let Some(answer) = ladder.verdict(
        &manager,
        provider,
        request_model.as_deref(),
        now,
        bound_to,
        strict_group,
    ) {
        return answer;
    }
    exhausted_response(
        &manager,
        now,
        provider_account_count,
        request_is_fable,
        provider,
        strict_group,
        bound_to,
    )
}

/// Why [`read_capped_body`] stopped short of a full buffer.
enum BodyReadError {
    /// The stream yielded a transport error before completing.
    Transport,
    /// The accumulated body would exceed the byte cap.
    TooLarge,
}

/// Buffer a byte stream into `Bytes`, aborting once the accumulated size would
/// exceed `cap`. Mirrors the request-path [`MAX_BODY_BYTES`] cap on the response
/// path: `resp.bytes().await` reads the whole upstream body with no bound, the
/// same DoS surface the inbound buffer guards against. A body whose total equals
/// the cap is accepted; only a strictly larger one is rejected.
async fn read_capped_body<S, E>(stream: S, cap: usize) -> Result<Bytes, BodyReadError>
where
    S: futures::Stream<Item = Result<Bytes, E>>,
{
    futures::pin_mut!(stream);
    let mut collected: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| BodyReadError::Transport)?;
        if collected.len().saturating_add(chunk.len()) > cap {
            return Err(BodyReadError::TooLarge);
        }
        collected.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(collected))
}

/// The most of a 2xx body [`sniff_sse`] holds before it gives up and calls it not a stream.
const SSE_SNIFF_CAP: usize = 4096;

/// Whether a body's first bytes are a Server-Sent Events stream: `Some(true)` when its first
/// non-blank line opens with an SSE field (`event:`, `data:`, `id:`, `retry:`) or a `:`
/// comment, `Some(false)` when it cannot, and `None` while too few bytes have arrived to say.
fn sse_verdict(head: &[u8]) -> Option<bool> {
    const FIELDS: [&[u8]; 5] = [b":", b"data:", b"event:", b"id:", b"retry:"];
    let start = head.iter().position(|b| !b.is_ascii_whitespace())?;
    let rest = &head[start..];
    if FIELDS.iter().any(|field| rest.starts_with(field)) {
        Some(true)
    } else if FIELDS.iter().any(|field| field.starts_with(rest)) {
        // A field name still arriving across chunks: `ev` of `event:`.
        None
    } else {
        Some(false)
    }
}

/// Read a 2xx body's first chunks until [`sse_verdict`] can say whether it is an SSE stream,
/// for a response whose `Content-Type` did not say `text/event-stream`. Codex streams arrive
/// that way: a 127 KB Codex turn went down the buffered arm, failed its JSON parse at the first
/// byte and was booked with no tokens (2026-10-17). Returns the verdict and the chunks read,
/// errors included, so the caller replays them ahead of the rest of the body.
async fn sniff_sse<S, E>(stream: &mut S) -> (bool, Vec<Result<Bytes, E>>)
where
    S: futures::Stream<Item = Result<Bytes, E>> + Unpin,
{
    let mut read = Vec::new();
    let mut head: Vec<u8> = Vec::new();
    while head.len() < SSE_SNIFF_CAP {
        let Some(chunk) = stream.next().await else {
            break;
        };
        let Ok(bytes) = &chunk else {
            read.push(chunk);
            break;
        };
        head.extend_from_slice(bytes);
        read.push(chunk);
        if let Some(verdict) = sse_verdict(&head) {
            return (verdict, read);
        }
    }
    (false, read)
}

/// Log a non-2xx upstream RESPONSE body: `error.type` and `error.message` pulled
/// out as their own greppable fields (Anthropic's error envelope is
/// `{"type":"error","error":{"type":...,"message":...}}`), plus the body itself
/// capped at [`ERROR_BODY_LOG_CAP`] for the shapes that don't parse (an upstream
/// 5xx can arrive as an HTML error page, not JSON). Fixes the blind spot logged
/// in `src/proxy.rs`'s "upstream response" line, which fires before this body
/// is even read and so has never carried more than the bare status code — 64
/// `status=400` lines on 2026-09-07 with no way to tell one 400 from another.
///
/// Response bodies ONLY. A request body can carry the client's own prompt
/// content and must never reach a log line in this public repo; nothing here
/// is wired to one.
fn log_upstream_error_body(
    idx: usize,
    account_name: Option<&str>,
    status: StatusCode,
    body: &Bytes,
) {
    let capped = &body[..body.len().min(ERROR_BODY_LOG_CAP)];
    let body_text = String::from_utf8_lossy(capped);
    let parsed: Option<Value> = serde_json::from_slice(body).ok();
    let error_type = parsed
        .as_ref()
        .and_then(|v| v.get("error"))
        .and_then(|e| e.get("type"))
        .and_then(|t| t.as_str());
    let error_message = parsed
        .as_ref()
        .and_then(|v| v.get("error"))
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str());
    tracing::warn!(
        account_index = idx,
        account = account_name.unwrap_or("?"),
        status = status.as_u16(),
        error.r#type = error_type.unwrap_or("<unparsed>"),
        error.message = error_message.unwrap_or("<unparsed>"),
        body_length = body.len(),
        body_truncated = body.len() > ERROR_BODY_LOG_CAP,
        body = %body_text,
        "upstream error response"
    );
}

/// Clone request headers for the upstream call, dropping hop-by-hop headers,
/// the client's `x-api-key`, its inbound `authorization` (behaviour #2), and
/// `accept-encoding` (reqwest has no decompressor here, and a compressed body
/// would defeat SSE usage parsing). Sets our own `Bearer`.
fn build_upstream_headers(req_headers: &HeaderMap, token: &str) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in req_headers {
        // `as_str`, and named for what it is. It was called `lower`, which
        // claimed a lowercasing this line does not do: it holds only because
        // `HeaderName` is already lowercase, so the name promised a property
        // the code was relying on someone else for.
        let name_str = name.as_str();
        if name_str.starts_with(':')
            || is_request_hop_by_hop(name_str)
            || name_str == "x-api-key"
            // The request key: a credential for this proxy, never for upstream.
            || name_str == crate::request_key::HEADER
            || name_str == "authorization"
            || name_str == "accept-encoding"
            // This Mac's own routing marker, which upstream has no use for and
            // which describes the mesh rather than the request.
            || name_str == RELAYED_HEADER_NAME
        {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    if let Ok(value) = HeaderValue::from_str(&format!("Bearer {token}")) {
        out.insert(AUTHORIZATION, value);
    }
    out
}

/// Does this request ask to become a WebSocket? Codex opens its Responses stream over a
/// WebSocket by default (`openai/codex` f82e365, `model-provider-info/src/lib.rs:561`); tcr
/// relays HTTP only.
pub(crate) fn wants_websocket_upgrade(headers: &HeaderMap) -> bool {
    headers
        .get("upgrade")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("websocket"))
}

/// The answer that moves Codex to SSE AT ONCE, for the rest of the session: a 426. At
/// `openai/codex` f82e365, `core/src/client.rs:2010-2014` matches `StatusCode::UPGRADE_REQUIRED`
/// exactly and returns `WebsocketStreamOutcome::FallbackToHttp`. Any other answer costs retries
/// first: codex falls back too once a WebSocket that keeps failing has spent its stream retry
/// budget (`core/src/responses_retry.rs:120-138`), and ends the turn on an error it does not
/// retry. Answered locally, before any account is selected, so an upgrade never costs a slot.
///
/// Built QUIETLY ([`quiet_error_response`]): every Codex session opens with an upgrade, so a
/// warn here would be one false alarm per session. `handle` logs it at debug.
pub(crate) fn upgrade_required_response() -> Response {
    ClientError::new(
        StatusCode::UPGRADE_REQUIRED,
        ErrorKind::UpgradeRequired,
        "tcr relays Codex over HTTP streaming only; the client falls back to SSE for this session.",
    )
    .respond_quiet(Provider::Codex)
}

/// Does this Codex request name a workspace routing of its own
/// ([`crate::provider::CODEX_ROUTING_HEADERS`])? Such a request comes from a ChatGPT login whose
/// workspace is served by another backend origin, or under a residency or FedRAMP routing that
/// no pooled account shares, so `handle` relays it on the client's own credential and never
/// pools it. Present is enough, whatever the value: codex adds the override only for a `us` or
/// `us_cr` residency and the marker only for a FedRAMP login (`openai/codex` f82e365,
/// `model-provider/src/workspace_routing.rs:12,39-77`,
/// `model-provider/src/bearer_auth_provider.rs:31-47`), so a workspace served at `chatgpt.com`
/// sends neither. Pure — unit-tested through `handle`.
fn names_codex_routing(headers: &HeaderMap) -> bool {
    crate::provider::CODEX_ROUTING_HEADERS
        .iter()
        .any(|name| headers.contains_key(*name))
}

/// [`build_upstream_headers`] plus the Codex identity swap. codex sends its bearer with its own
/// account's id in `chatgpt-account-id` (`openai/codex` f82e365,
/// `model-provider/src/bearer_auth_provider.rs:31-47`), so the client's header names ITS
/// account; tcr replaces it with the serving account's, so a pooled bearer always travels with
/// its own account's id. What the backend answers a bearer paired with another account's id is
/// unmeasured. The body is NOT touched — it is usually zstd-compressed
/// (`core/src/client.rs:1704-1711`) and the identity lives in this header, not in a
/// `metadata.user_id` field.
///
/// The client's routing headers ([`crate::provider::CODEX_ROUTING_HEADERS`]) are dropped too: a
/// request carrying one is relayed before it reaches the pool ([`names_codex_routing`]), and a
/// pooled bearer must never travel under another workspace's routing even if one slipped by.
///
/// The client's `x-codex-turn-state`, codex's sticky-routing token, is forwarded as it came, on
/// every attempt. codex takes it from a turn's first response, replays it on every later request
/// of that turn and clears it when its auth owner changes (`openai/codex` f82e365,
/// `core/src/client.rs:11-13,163,282-309,1661-1665,2046-2047,2420-2436`), so after a rotation
/// inside one request (on a 429, a 403, a second 401 or a transport failure) the account serving
/// the retry receives a token minted for another account. How the backend treats that is
/// unmeasured, so the header is not stripped on that ground; if the backend is ever seen refusing
/// one, strip it whenever the serving account is not the one the session is pinned to.
pub(crate) fn build_codex_upstream_headers(
    req_headers: &HeaderMap,
    token: &str,
    account_id: Option<&str>,
) -> HeaderMap {
    let mut out = build_upstream_headers(req_headers, token);
    out.remove(crate::provider::CODEX_ACCOUNT_HEADER);
    for name in crate::provider::CODEX_ROUTING_HEADERS {
        out.remove(name);
    }
    if let Some(id) = account_id
        && let Ok(value) = HeaderValue::from_str(id)
    {
        out.insert(
            HeaderName::from_static(crate::provider::CODEX_ACCOUNT_HEADER),
            value,
        );
    }
    out
}

/// Only the observed ChatGPT model-access rejection is eligible for failover. Other
/// bad parameters, HTML challenges and malformed bodies keep their original response.
fn codex_model_refusal_detail(bytes: &[u8]) -> Option<String> {
    if bytes.len() > RATE_LIMIT_BODY_CAP {
        return None;
    }
    let body: Value = serde_json::from_slice(bytes).ok()?;
    let message = [
        "/detail",
        "/detail/message",
        "/error/message",
        "/message",
        "/error",
    ]
    .iter()
    .find_map(|path| body.pointer(path).and_then(Value::as_str))?;
    let normalized = message
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    normalized
        .contains("model is not supported when using codex with a chatgpt account")
        .then(|| message.chars().take(2048).collect())
}

/// A model-specific refusal is actionable without retrying or replacing a healthy login.
/// Describe this request's observed rejections, rather than inferring a permanent catalog
/// from plan names. A supporting account may simply be unavailable right now.
fn codex_model_unavailable_response(
    manager: &Manager,
    model: Option<&str>,
    refused: &[(Option<String>, String)],
    credential_refusals: &[(usize, Option<String>)],
) -> Response {
    let model: String = model
        .unwrap_or("the requested model")
        .chars()
        .take(256)
        .collect();
    let accounts = refused
        .iter()
        .map(|(name, _)| name.as_deref().unwrap_or("<unnamed>"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut message = format!(
        "tcr could not serve {model} from its available pooled Codex accounts. ChatGPT rejected that model for {accounts}. Choose another model or make an account that supports it available. {}",
        refused
            .first()
            .map_or_default(|(_, detail)| detail.as_str()),
    );
    if !credential_refusals.is_empty() {
        message.push(' ');
        message.push_str(&codex_credential_refused_message(credential_refusals));
    }
    manager.note_codex_refusal(CodexRefusalKind::ModelUnavailable, crate::now_ms());
    codex_error_response(
        StatusCode::BAD_REQUEST,
        codex_error_body(CodexRefusalKind::ModelUnavailable, message).to_string(),
        None,
    )
}

/// The common Codex error envelope, with its type and code drawn from one refusal kind.
pub(crate) fn codex_error_body(kind: CodexRefusalKind, message: impl Into<String>) -> Value {
    serde_json::json!({
        "error": {
            "type": kind.error_type(),
            "code": kind,
            "message": message.into(),
        }
    })
}

/// Every tcr-generated Codex refusal uses the same JSON response headers. Only a refusal
/// that waiting can end carries a retry hint; the body keeps its own provider-specific detail.
pub(crate) fn codex_error_response(
    status: StatusCode,
    body: String,
    retry_after: Option<i64>,
) -> Response {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    if let Some(seconds) = retry_after
        && let Ok(value) = HeaderValue::from_str(&seconds.to_string())
    {
        response.headers_mut().insert("retry-after", value);
    }
    response
}

/// The 429 body a Codex client gets when the pool is out on a quota
/// ([`codex_limit_is_quota`]: the gate that times the wait is a five-hour or weekly window or a
/// refusal, or, with no gate timing it, every account out is out on one of those), which is the
/// honest message then: a usage limit, with when it ends if any account reported that. A pool
/// out on anything else gets [`codex_pool_unavailable_body`].
///
/// Read against openai/codex f82e365. Codex does not retry a 429 at the transport layer
/// (`retry_429: false`, set in `ModelProviderInfo::to_api_provider`,
/// `codex-rs/model-provider-info/src/lib.rs:67,454-460`), but its turn loop retries any other
/// 429 after its `Retry-After`, up to five times (`core/src/responses_retry.rs:140-166`), and
/// it ends the turn on this one (`core/src/session/turn.rs:1694-1699`). It parses this body in
/// `map_api_error` (`codex-rs/codex-api/src/api_bridge.rs`): an `error.type` of
/// `usage_limit_reached` becomes its usage-limit error, and `error.resets_at`,
/// absolute Unix seconds, is the reset it prints ("Try again at 3:45 PM"; without it, "Try
/// again later"). So `resets_at` is written only when `reset_measured`, that is when some
/// account reported when it frees, and it is then `now` plus the wait, the instant
/// `resets_in_seconds` counts down to. Without a measured reset the wait is the 60-second
/// fallback, which codex would print as a time nobody measured, and the message says so. No
/// codex code reads `resets_in_seconds` at that commit; it stays for a person reading the body.
/// `plan_type` stays null: a pool can mix plans, and codex words its advice by plan.
pub(crate) fn codex_exhausted_body(
    now: OffsetDateTime,
    resets_in_seconds: u64,
    reset_measured: bool,
) -> String {
    let message = if reset_measured {
        format!(
            "No pooled Codex account can take this request now; the first one frees in {resets_in_seconds} seconds."
        )
    } else {
        format!(
            "No pooled Codex account can take this request now, and none reports when it frees; retry in {resets_in_seconds} seconds."
        )
    };
    let mut body = codex_error_body(CodexRefusalKind::UsageLimitReached, message);
    let error = &mut body["error"];
    error["resets_in_seconds"] = resets_in_seconds.into();
    error["plan_type"] = Value::Null;
    if reset_measured {
        error["resets_at"] = now
            .unix_timestamp()
            .saturating_add(i64::try_from(resets_in_seconds).unwrap_or(i64::MAX))
            .into();
    }
    body.to_string()
}

/// Is the pool out on a QUOTA, so that its 429 may tell a Codex client its usage limit is
/// reached ([`codex_exhausted_body`])? Yes when the gate that times the wait is a five-hour or
/// weekly window or the backend's own refusal (`FiveHour`, `SevenDay`, `Rejected`), or, with no
/// gate timing it, when every account out is out on one of those. Anything else (a hold on an
/// upstream `retry-after`, a dead login, an account disabled, parked or reserved) is a wait or
/// an operator's matter, not the user's limit, and gets [`codex_pool_unavailable_body`]: codex
/// ends its turn on `usage_limit_reached` but retries any other 429 after its `retry-after`.
///
/// `gated` must be non-empty. The 60-second sentinel can come with nothing in it, and "every
/// reason is a quota" holds vacuously of nothing. Pure — unit-tested.
fn codex_limit_is_quota(hint: &ExhaustionHint) -> bool {
    let quota = |reason: &GateReason| {
        matches!(
            reason,
            GateReason::FiveHour | GateReason::SevenDay | GateReason::Rejected
        )
    };
    match hint.binding {
        Some(binding) => quota(&binding),
        None => !hint.gated.is_empty() && hint.gated.keys().all(quota),
    }
}

/// The 429 body a Codex client gets when the pool is out on anything else
/// ([`codex_limit_is_quota`] is false): the Anthropic arm's plain 429, in codex's envelope.
/// The same status and `{"error": {...}}` as [`codex_exhausted_body`], under a tcr `type` and
/// `code`, `pool_unavailable`, that is none of the three codex reads as a limit
/// (`usage_limit_reached`, `usage_not_included`, `insufficient_quota`), and with no `resets_at`
/// or `plan_type`: the wait is tcr's, not a window for codex to print. codex's turn loop
/// retries such a 429 after its `retry-after`, up to five times
/// (`core/src/responses_retry.rs:140-166`, `protocol/src/error.rs:427-429` at openai/codex
/// f82e365), which `exhausted_response` sets as it does for a Claude client.
/// `resets_in_seconds` is `wait_secs`, the uncapped wait, as the usage-limit body carries it:
/// no codex code reads it, and it is there for a person or a script reading the body.
/// `wait_measured` is whether some account reports when it frees, without which the wait is the
/// 60-second fallback and the message says so.
pub(crate) fn codex_pool_unavailable_body(wait_secs: u64, wait_measured: bool) -> String {
    let message = if wait_measured {
        format!(
            "No pooled Codex account can serve this request right now; the soonest frees in {wait_secs} seconds."
        )
    } else {
        format!(
            "No pooled Codex account can serve this request right now, and none reports when it frees; retry in {wait_secs} seconds."
        )
    };
    let mut body = codex_error_body(CodexRefusalKind::PoolUnavailable, message);
    body["error"]["resets_in_seconds"] = wait_secs.into();
    body.to_string()
}

/// The error body for a missing Codex pool. It is sent as a 400, because Codex retries
/// every 429 regardless of its retry-after header, and waiting cannot create a login.
pub(crate) fn codex_no_account_body() -> String {
    codex_error_body(
        CodexRefusalKind::NoCodexAccount,
        "No Codex account is configured in tcr; run `tcr login --codex` to add one.",
    )
    .to_string()
}

/// `hand` mode, and the ONLY bearer on this request path that is
/// not this Mac's own: the owner of a leased account handed this node a
/// short-lived access token over an `IK`-authenticated CONTROL session, and a
/// request served under that lease leaves from HERE rather than from the
/// owner's Mac.
///
/// # The third arm, beside the pooled bearer and the client's own
///
/// [`build_upstream_headers`] sets this Mac's pooled `Bearer` for the rotation
/// loop; [`build_passthrough_headers`] keeps the CLIENT's own `authorization`
/// for [`RelayMode::ClientCredential`]. This one sets a bearer that belongs to
/// neither: a peer owner's. It reuses `build_upstream_headers` for the stripping
/// rather than repeating the drop list, because a second copy of that list is
/// how an inbound `authorization` survives on exactly one of the three paths.
///
/// # Where the token comes from, and where it can never go
///
/// [`crate::peer::lease::handed_tokens`], a process-local map with no `serde`
/// and no path. `None` here means no live handed bearer for that lease, which
/// the caller answers by NOT sending: a hand-mode request with no token is a
/// request this node cannot pay for, and the ladder's next rung is a better
/// answer than an unauthenticated call to upstream.
pub(crate) fn build_handed_bearer_headers(
    req_headers: &HeaderMap,
    lease_id: u128,
    now_ms: i64,
) -> Option<HeaderMap> {
    let store = crate::peer::lease::handed_tokens().lock().ok()?;
    let token = store.bearer(lease_id, now_ms)?;
    Some(build_upstream_headers(req_headers, token))
}

/// Clone the client's request headers for a [`RelayMode::ClientCredential`] call:
/// drop pseudo-headers, hop-by-hop and `accept-encoding`, KEEP everything else —
/// crucially the client's own `authorization`, which is the entire point of this
/// path. Contrast [`build_upstream_headers`], which strips exactly that header to
/// make room for the pooled Bearer.
///
/// Removed beyond that set: the request key header ([`crate::request_key::HEADER`]),
/// and an `x-api-key` whose value equals either proxy secret ([`ProxySecrets`]).
/// Those are OUR credentials, not the client's upstream credential, and forwarding
/// one hands the operator's secret to a third party. A client `x-api-key` that is
/// NOT one of ours is the client's own and passes through.
fn build_passthrough_headers(req_headers: &HeaderMap, secrets: ProxySecrets<'_>) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in req_headers {
        // Named for what it is, not for a lowercasing this line does not do.
        // See `build_upstream_headers`.
        let name_str = name.as_str();
        if name_str.starts_with(':')
            || is_request_hop_by_hop(name_str)
            || name_str == "accept-encoding"
            || name_str == crate::request_key::HEADER
        {
            continue;
        }
        if name_str == "x-api-key" && secrets.contains(value.to_str().ok()) {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    out
}

/// The three headers a [`RelayMode::Raw`] relay carries — `content-type`, `accept`,
/// `user-agent` — and nothing else. No `authorization` and no `x-api-key`: a token
/// refresh authenticates with the credentials in its BODY, so a header credential
/// would be at best redundant and, for a pooled Bearer, the wrong identity's
/// entirely.
fn build_raw_relay_headers(req_headers: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, default) in [
        (CONTENT_TYPE, "application/json"),
        (ACCEPT, "application/json"),
        (USER_AGENT, RELAY_USER_AGENT),
    ] {
        let value = req_headers
            .get(&name)
            .cloned()
            .unwrap_or_else(|| HeaderValue::from_static(default));
        out.insert(name, value);
    }
    out
}

/// The HTTP client [`relay_upstream`] uses, built once and reused.
///
/// Deliberately NOT [`Manager::http_client`]: the pooled client follows up to 10
/// redirects, and a relay must hand a 3xx BACK to the client rather than chase it
/// (a manual-redirect policy). Following one here
/// would resolve a location the client never sees, while carrying a credential we
/// are only forwarding on its behalf. The rest mirrors the pooled client: `no_proxy`
/// (we ARE the proxy — an ambient `HTTP_PROXY` would loop us through ourselves) and
/// a CONNECT-phase-only timeout, never a total one, because a `/v1/code/` response
/// is a long-lived event stream that any total timeout would truncate mid-stream.
///
/// Rebuilt on the first relay after the machine slept, for the reason the
/// per-account clients are ([`crate::wake`]): its pooled connections died during
/// the sleep and its idle timer did not move.
///
/// `None` only if the TLS backend fails to initialise, which the caller answers with
/// a 502 — a proxy never panics on a client request. The next relay tries the build
/// again.
///
/// In a test build only, a test whose upstream is a stub gets a fresh client with the same
/// settings that sends over the stubs' Unix socket (`test_support::stub_upstream`, § The Unix
/// socket); every other caller gets the shared one.
fn relay_client() -> Option<Arc<reqwest::Client>> {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(socket) = crate::test_support::stub_upstream::client_socket() {
        return build_relay_client(relay_client_builder().unix_socket(socket)).map(Arc::new);
    }
    RELAY_CLIENT.get()
}

/// [`RELAY_CLIENT`]'s settings, before `build`.
fn relay_client_builder() -> reqwest::ClientBuilder {
    crate::http_client::builder()
        .no_proxy()
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
}

fn build_relay_client(builder: reqwest::ClientBuilder) -> Option<reqwest::Client> {
    match builder.build() {
        Ok(client) => Some(client),
        Err(err) => {
            tracing::error!(error = %err, "could not build the relay HTTP client");
            None
        }
    }
}

static RELAY_CLIENT: std::sync::LazyLock<crate::wake::SleepAwareClient<reqwest::Client>> =
    std::sync::LazyLock::new(|| {
        crate::wake::SleepAwareClient::new("relay", || build_relay_client(relay_client_builder()))
    });

/// Forward a request upstream WITHOUT touching account rotation — see the `1c`
/// block in [`handle`] for which requests land here and why.
///
/// `mode` picks the header policy ([`build_passthrough_headers`] vs
/// [`build_raw_relay_headers`]) and the body policy: a client-credential response is
/// STREAMED (`/v1/code/` is an event stream that must not be buffered before the
/// client sees it), a raw response is buffered under the same cap as every other
/// body this file reads. Response headers go through the shared [`build_response`],
/// which drops the framing headers a relayed body would mis-frame with.
///
/// `provider` is the request's own, derived by [`handle`] from the query-stripped
/// path, and picks the origin: a Codex relay (`/backend-api/wham/usage`, every
/// other non-inference Codex path, and a request that names a workspace routing of
/// its own, inference included) reaches the Codex upstream with the client's own
/// bearer, never Anthropic's.
/// `arrived` is [`handle`]'s clock, so the `relay timing` line's `total_ms` counts
/// the gates the request passed before it got here. A streamed relay body takes
/// the clock with it and logs when it ends; everything else logs here.
#[expect(
    clippy::too_many_arguments,
    reason = "one relayed request's parts, passed through unchanged"
)]
async fn relay_upstream(
    manager: &Manager,
    mode: RelayMode,
    provider: Provider,
    method: Method,
    path_and_query: &str,
    req_headers: &HeaderMap,
    body: Body,
    arrived: Instant,
) -> Response {
    let mut timing = Some(RelayTiming {
        arrived,
        path: path_and_query.to_owned(),
        mode,
        send_started: None,
        headers_at: None,
        stream: false,
        ended_at: None,
    });
    let response = relay_request(
        manager,
        mode,
        provider,
        method,
        path_and_query,
        req_headers,
        body,
        &mut timing,
    )
    .await;
    if let Some(timing) = timing {
        timing.emit(response.status().as_u16(), Instant::now(), true);
    }
    response
}

/// [`relay_upstream`]'s body. `timing` is taken (left `None`) by a streamed
/// response, whose body then owns the `relay timing` line.
#[expect(
    clippy::too_many_arguments,
    reason = "one relayed request's parts, passed through unchanged"
)]
async fn relay_request(
    manager: &Manager,
    mode: RelayMode,
    provider: Provider,
    method: Method,
    path_and_query: &str,
    req_headers: &HeaderMap,
    body: Body,
    timing: &mut Option<RelayTiming>,
) -> Response {
    let Some(client) = relay_client() else {
        return ClientError::new(
            StatusCode::BAD_GATEWAY,
            ErrorKind::Proxy,
            "Could not build the relay HTTP client.",
        )
        .respond(provider);
    };
    let Ok(body_bytes) = to_bytes(body, MAX_BODY_BYTES).await else {
        return ClientError::new(
            StatusCode::BAD_REQUEST,
            ErrorKind::InvalidRequest,
            "Failed to read request body.",
        )
        .respond(provider);
    };

    let headers = match mode {
        RelayMode::ClientCredential => {
            build_passthrough_headers(req_headers, manager.proxy_secrets())
        }
        RelayMode::Raw => build_raw_relay_headers(req_headers),
    };
    let url = format!("{}{}", manager.upstream_for(provider), path_and_query);
    let mut builder = client.request(method.clone(), &url).headers(headers);
    // A GET/HEAD carries no body, and an empty body is sent as no body at all,
    // which keeps a bodyless POST from acquiring a `content-length: 0` it did
    // not arrive with.
    if method != Method::GET && method != Method::HEAD && !body_bytes.is_empty() {
        builder = builder.body(body_bytes);
    }

    if let Some(timing) = timing.as_mut() {
        timing.send_started = Some(Instant::now());
    }
    let resp = match builder.send().await {
        Ok(resp) => {
            if let Some(timing) = timing.as_mut() {
                timing.headers_at = Some(Instant::now());
            }
            resp
        }
        Err(err) => {
            tracing::warn!(
                path = %path_and_query,
                mode = ?mode,
                is_connect = err.is_connect(),
                is_timeout = err.is_timeout(),
                // `?err`, not `%err`: reqwest's Display prints only its top
                // frame ("error sending request for url (...)"), so the cause
                // that tells a reset from a refusal would be lost.
                error = ?err,
                "relay transport failure"
            );
            // A failure that never left this box answers the way the main path
            // does: a dead resolver is the machine being offline, any other
            // connect-phase failure is safe to retry. Both checks run in this
            // order because a DNS failure is also `is_connect()`. Past connect,
            // the request may have reached the upstream, so it keeps the plain
            // 502 with no `x-should-retry`: telling a client to retry it is a
            // decision about relayed paths this arm does not make.
            if is_offline_error(&err) {
                return offline_unavailable(provider, 1);
            }
            if err.is_connect() {
                return bad_gateway(provider, 1, false);
            }
            return ClientError::new(
                StatusCode::BAD_GATEWAY,
                ErrorKind::Proxy,
                "Upstream unreachable.",
            )
            .respond(provider);
        }
    };

    let status = resp.status();
    let up_headers = resp.headers().clone();
    // The relay's twin of the "upstream response" line. These requests are invisible
    // to the per-account log by construction (they serve no account), so this line and
    // the DEBUG `relay timing` line are the only record that one happened and what it
    // answered. Same rule as the pooled line: INFO when the answer was not a success,
    // DEBUG otherwise, because relayed bookkeeping calls are most of a client's requests
    // and one INFO line each was nearly a quarter of the log's per-request lines.
    if status.is_success() {
        tracing::debug!(
            path = %path_and_query,
            mode = ?mode,
            status = status.as_u16(),
            "relayed response"
        );
    } else {
        tracing::info!(
            path = %path_and_query,
            mode = ?mode,
            status = status.as_u16(),
            "relayed response"
        );
    }

    match mode {
        // Both arms are served with the CALLER's own credential (or none at all),
        // so their rate-limit and org headers describe the caller — coherent, and
        // theirs to see. Only the rotated path lies; see [`ServedBy`].
        RelayMode::ClientCredential => {
            // The clock rides in the body so `total_ms` is the stream's end, not
            // the moment its headers were forwarded. Taken, so the caller does
            // not log this request a second time.
            let body = match timing.take() {
                Some(mut owned) => {
                    owned.stream = true;
                    let clock = RelayStreamTiming {
                        timing: owned,
                        status: status.as_u16(),
                    };
                    Body::from_stream(futures::stream::unfold(
                        (resp.bytes_stream(), clock),
                        |(mut inner, mut clock)| async move {
                            if let Some(item) = inner.next().await {
                                if item.is_err() {
                                    clock.mark_ended();
                                }
                                Some((item, (inner, clock)))
                            } else {
                                clock.mark_ended();
                                None
                            }
                        },
                    ))
                }
                None => Body::from_stream(resp.bytes_stream()),
            };
            build_response(status, &up_headers, body, ServedBy::Caller)
        }
        RelayMode::Raw => match read_capped_body(resp.bytes_stream(), MAX_BODY_BYTES).await {
            Ok(bytes) => build_response(status, &up_headers, Body::from(bytes), ServedBy::Caller),
            Err(BodyReadError::Transport) => ClientError::new(
                StatusCode::BAD_GATEWAY,
                ErrorKind::Proxy,
                "Failed to read upstream response body.",
            )
            .respond(provider),
            Err(BodyReadError::TooLarge) => ClientError::new(
                StatusCode::BAD_GATEWAY,
                ErrorKind::Proxy,
                "Upstream response body exceeded the size cap.",
            )
            .respond(provider),
        },
    }
}

/// Hop-by-hop request headers that must not be forwarded. Header names from a
/// [`HeaderMap`] are already lowercased.
///
/// `pub(crate)` so `src/peer/serve.rs` asks THIS predicate rather than holding a
/// second eleven-name list. A SERVE frame is a request leaving this host for
/// another one, so every name here is wrong on it for exactly the reasons it is
/// wrong upstream, and `proxy-authorization` is a credential as well as a
/// hop-by-hop header, which is the one this list must not be allowed to drift on.
pub(crate) fn is_request_hop_by_hop(name: &str) -> bool {
    matches!(
        name,
        "host"
            | "connection"
            | "keep-alive"
            | "transfer-encoding"
            | "te"
            | "trailer"
            | "trailers"
            | "upgrade"
            | "proxy-authorization"
            | "proxy-authenticate"
            | "content-length"
    )
}

/// Response headers that are connection-specific / would mis-frame the relayed
/// body and so are stripped from the response we return to the client.
///
/// `pub(crate)` alongside [`is_request_hop_by_hop`], so a SERVE reply rebuilt in
/// `src/peer/serve.rs` strips exactly what this handler strips. A relayed
/// response that kept its `content-length` would mis-frame the body the same way
/// here as it does there.
///
/// **Compared case-insensitively, because two of its callers are fed arbitrary
/// strings off a peer's wire** (`peer::serve::response_from` and the hand arm
/// in `peer::lease`), not `HeaderName`s this process built. It used to be an
/// exact lowercase match, which held only because every lender in this fleet
/// writes `HeaderName::as_str()`: a peer that spelled one `Content-Length`
/// walked past the gate and mis-framed the body in its borrower's client. A
/// header name is case-insensitive per the HTTP grammar, so trusting a peer for
/// the case was trusting it for the rule.
pub(crate) fn is_response_skip(name: &str) -> bool {
    RESPONSE_SKIP
        .iter()
        .any(|skip| name.eq_ignore_ascii_case(skip))
}

/// The names [`is_response_skip`] answers `true` to, as data rather than as a
/// `matches!` arm, so the comparison above can be the case-insensitive one the
/// HTTP grammar asks for.
const RESPONSE_SKIP: [&str; 10] = [
    "connection",
    "keep-alive",
    "transfer-encoding",
    "upgrade",
    "proxy-connection",
    "te",
    "trailer",
    "trailers",
    "content-length",
    "content-encoding",
];

/// Upstream headers that describe the ACCOUNT that served a request rather than
/// the request itself, and so are only meaningful to a client that owns that
/// account.
///
/// Matched by PREFIX, deliberately: the `anthropic-ratelimit-` family has more
/// members than the `unified-*` set this proxy reads (`requests-*`, `tokens-*`,
/// `input-tokens-*`, `output-tokens-*`, …), and an enumeration would leak
/// whichever one Anthropic adds next. Header names from a [`HeaderMap`] are
/// already lowercased.
///
/// Codex's per-account headers are the opposite case and are matched by EXPLICIT
/// name, the eleven in [`crate::quota::CODEX_QUOTA_HEADERS`], never by the `x-codex-`
/// prefix: `x-codex-turn-state` shares that prefix, and it is the client's own
/// sticky-routing token, which must reach the client.
///
/// `request-id` is deliberately absent: it identifies a REQUEST, not an account,
/// and it is the one id that makes a single failed call debuggable end-to-end.
///
/// `pub(crate)` for the same reason [`is_response_skip`] is: the peer-egress
/// path serves a response to the SAME local client off the SAME pooled account,
/// so a second copy of this rule over there would be a second answer to "what
/// may the caller see", and it would drift the first time Anthropic adds a
/// header. The other reader is `crate::peer::tunnel::egress`'s `axum_response_from`.
pub(crate) fn is_account_scoped(name: &str) -> bool {
    name.starts_with("anthropic-ratelimit-")
        || name == "anthropic-organization-id"
        || crate::quota::CODEX_QUOTA_HEADERS.contains(&name)
}

/// Whose account produced the response being assembled — the one thing that
/// decides whether its per-account headers mean anything to the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServedBy {
    /// A pooled account picked by rotation. Its quota headers describe an account
    /// the caller has never heard of and that CHANGES per request, so
    /// [`is_account_scoped`] strips them on the way out — see [`build_response`].
    PooledAccount,
    /// The caller's own credential ([`RelayMode`] paths, which bypass rotation
    /// entirely). Those headers describe the caller's own account, so they are
    /// coherent and pass through untouched.
    Caller,
}

/// Assemble the client response: the upstream status + body, carrying every
/// upstream header except the connection-specific / framing ones — and, on the
/// rotated path, except the ones that belong to the serving ACCOUNT.
///
/// The account strip exists because Claude Code renders its usage UI straight
/// from `anthropic-ratelimit-unified-*`. Rotation means consecutive requests are
/// answered by different accounts, so forwarding those headers hands the client a
/// different quota picture every time: a request that lands on an account at 1.00
/// weekly renders a usage-limit banner, the next one lands elsewhere and
/// contradicts it. None of those numbers describe the pool the client is actually
/// talking to. `anthropic-organization-id` is stripped with them — it leaks a
/// distinct org identity per pooled account.
///
/// Codex's quota headers go for the same reason (the Codex client shows the serving
/// account's usage from them), by explicit name: `x-codex-turn-state` is the client's
/// own and passes. See [`is_account_scoped`].
///
/// This is the CLIENT boundary, and the strip belongs here and nowhere earlier:
/// the proxy's own quota model is built from these same headers upstream of this
/// call — `manager.update_quota` and the `quota_rejections` gate both read the
/// untouched `up_headers`. Stripping at ingest would blind the rotation logic
/// while looking like it fixed something.
fn build_response(
    status: StatusCode,
    up_headers: &HeaderMap,
    body: Body,
    served_by: ServedBy,
) -> Response {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    let headers = response.headers_mut();
    for (name, value) in up_headers {
        if is_response_skip(name.as_str()) {
            continue;
        }
        if served_by == ServedBy::PooledAccount && is_account_scoped(name.as_str()) {
            continue;
        }
        headers.append(name.clone(), value.clone());
    }
    response
}

#[cfg(test)]
fn is_quota_rejected(headers: &HeaderMap) -> bool {
    !quota_rejections(headers).is_empty()
}

/// Sum the input side of a `usage` object: base input plus cache-creation and
/// cache-read tokens.
/// This is the QUOTA counter — `input_total` folds all three into one u64.
fn sum_input_tokens(usage: &Value) -> u64 {
    let field = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    field("input_tokens") + field("cache_creation_input_tokens") + field("cache_read_input_tokens")
}

/// The parsed token breakdown of a response. `input_total` keeps
/// [`sum_input_tokens`] semantics byte-for-byte (the quota counter — bug #4),
/// while `cache_read` / `cache_creation` are ALSO surfaced separately so an
/// operator can see prompt-cache warmth (`cache_read > 0` on post-first turns).
/// A single struct — rather than a 4-tuple — keeps the call sites legible.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ParsedUsage {
    input_total: u64,
    output: u64,
    cache_read: u64,
    cache_creation: u64,
    /// The part of `cache_creation` written under the extended 1-hour TTL. A
    /// SUBSET of `cache_creation`, never additional to it — the 5-minute part is
    /// `cache_creation - cache_creation_1h`. Kept apart because the two bill
    /// differently (2x base input against 1.25x), so a single cache-creation
    /// number cannot be priced.
    ///
    /// `None` means the response REPORTED NO SPLIT; `Some(0)` means it reported
    /// one and it was zero. Those are different facts and collapsing them is a
    /// pricing error, not a formality: a Claude Code session pins its system
    /// prompt at 1h and its conversation at 5m, so a turn writing 200k tokens
    /// of 5-minute cache and nothing at 1h answers `{ephemeral_5m: 200000,
    /// ephemeral_1h: 0}` — and reading that 0 as "no data" hands the whole
    /// write to the request's 1h flag and prices it at 2x instead of 1.25x.
    cache_creation_1h: Option<u64>,
    /// A Codex turn's reasoning tokens (`output_tokens_details.reasoning_tokens`): the part
    /// of `output` its model spent reasoning, already counted inside `output`. Always 0 for
    /// an Anthropic turn, whose usage reports no such split.
    reasoning: u64,
}

/// Extract the cache-read + cache-creation components of a `usage` object,
/// plus how much of the cache creation was written under the extended 1-hour
/// TTL.
/// R2: both parse paths (JSON and SSE) call THIS, so the three cache fields are
/// extracted identically and cache% can never depend on stream-mode.
///
/// The 1-hour figure comes from the nested `usage.cache_creation` object
/// (`ephemeral_1h_input_tokens`), which the Messages API sends alongside the
/// flat `cache_creation_input_tokens`. It is CLAMPED to the flat count on
/// purpose: the flat number is the one the existing `cache_creation_tokens`
/// counter has always carried, and splitting it must never change its total.
///
/// `None` — and ONLY `None` — means the response did not report a split at all
/// (no nested object, or no such field in it). A reported `0` comes back as
/// `Some(0)`: it is a first-hand statement that nothing was written at 1h, and
/// the caller must not treat it as missing data and fall back to the REQUEST's
/// `cache_control` ttl ([`crate::request_body::RequestBody::extended_ttl`]), which is
/// only evidence of what was ASKED for.
fn cache_breakdown(usage: &Value) -> (u64, u64, Option<u64>) {
    let field = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    let cache_creation = field("cache_creation_input_tokens");
    // The OBJECT is the report, not the 1h key inside it. A response that sends
    // `cache_creation: {"ephemeral_5m_input_tokens": 200000}` and no 1h key has
    // stated where its cache creation went; reading that as "no split reported"
    // hands the whole 200k to the request's `cache_control` flag and prices a
    // 5-minute write at the 1-hour rate. Only an ABSENT object is silence.
    let cache_creation_1h = usage
        .get("cache_creation")
        .filter(|c| c.is_object())
        .map(|c| {
            c.get("ephemeral_1h_input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .min(cache_creation)
        });
    (
        field("cache_read_input_tokens"),
        cache_creation,
        cache_creation_1h,
    )
}

/// Build the ledger record for one served request from its parsed response
/// usage, the model the REQUEST named, and whether the request asked for the
/// extended cache window.
///
/// Two things happen here that neither caller should duplicate.
///
/// **The model comes from the REQUEST, not the response.** It is parsed once per
/// request ([`crate::request_body::RequestBody::model`]) and is the same on both arms; the response's
/// own `model` field can differ from what was asked for, and the ask is what the
/// operator reasons about. One exception, and only when the request named no model: a
/// Codex body is usually zstd and then does not parse, so each arm passes the model its
/// response named, the stream's terminal event's ([`SseParse::model`]) for a streamed turn and
/// the Responses object's top-level `model` ([`NonStreamedResponse::model`]) otherwise, rather than file
/// the turn under "unknown".
///
/// **The 5m/1h split falls back to the request's `cache_control` ttl, and only
/// then.** When the response REPORTED
/// `usage.cache_creation.ephemeral_1h_input_tokens` that figure wins outright,
/// including when it reported zero — it is a first-hand report of what was
/// written, and a reported zero is data. Only when the response omitted the
/// field entirely (`None`) is `extended_ttl` consulted, and then it decides ALL
/// of the flat cache-creation count, because a body whose only `cache_control`
/// ttl is `"1h"` wrote its cache under that window. Silence in both places means
/// 5 minutes, which is the API's own default.
///
/// `extended_ttl` is a closure so that it is asked only when it is the only
/// evidence left: a response that reported its own split, and a response that
/// wrote no cache at all, both leave it uncalled. The handler's answer comes
/// from the request's one parse ([`crate::request_body::RequestBody::extended_ttl`]).
fn usage_record(
    parsed: &ParsedUsage,
    model: Option<String>,
    session: Option<u64>,
    extended_ttl: impl FnOnce() -> bool,
) -> crate::usage::UsageRecord {
    let cache_1h = match parsed.cache_creation_1h {
        Some(reported) => reported,
        // `cache_creation > 0` FIRST, and it is not a micro-optimisation: with
        // nothing written to the cache both arms produce 0, so asking the
        // question cannot change the record — and asking it costs a full body
        // deserialize, inline on the request path for a non-streamed response.
        None if parsed.cache_creation > 0 && extended_ttl() => parsed.cache_creation,
        None => 0,
    };
    let mut record = crate::usage::UsageRecord::from_quota_input(
        crate::now_ms(),
        model,
        session,
        // The QUOTA figure, verbatim: `input_total` is `sum_input_tokens`, the
        // number the cumulative counter has always been fed. The base input
        // priced at the input rate is derived from it inside the constructor,
        // which is also where a caller whose components exceed it is handled.
        parsed.input_total,
        parsed.cache_creation.saturating_sub(cache_1h),
        cache_1h,
        parsed.cache_read,
        parsed.output,
    );
    // A Codex turn's reasoning tokens (always 0 on an Anthropic turn), for the ledger line's
    // `rt` and every usage bucket's `reasoningTokens`.
    record.reasoning = parsed.reasoning;
    record
}

/// The provider-specific usage and response model. Kept separate so the usage
/// counters remain Copy while the model can own its name.
#[derive(Debug, Default)]
struct JsonParse {
    usage: ParsedUsage,
    model: Option<String>,
}

/// Every reading from one buffered 2xx body, including the response's own model.
/// Only Anthropic mints its bound-token kinds and `tool_use` blocks.
#[derive(Debug, Default)]
struct NonStreamedResponse {
    usage: ParsedUsage,
    model: Option<String>,
    tool_uses: Vec<crate::session_wire::ToolUseEvent>,
    bound_tokens: Vec<MintedToken>,
}

/// Parse once, then derive all provider-specific readings from that tree. A malformed
/// body yields no readings and logs only its length and the decoder diagnostic.
fn parse_non_streamed_response(bytes: &[u8], provider: Provider) -> NonStreamedResponse {
    let value = match serde_json::from_slice::<Value>(bytes) {
        Ok(value) => value,
        Err(err) => {
            tracing::warn!(
                error = %err,
                body_len = bytes.len(),
                "a non-streamed 200 body did not parse as JSON, so this turn's tokens are \
                 NOT in the usage ledger; totals for this window are an undercount"
            );
            return NonStreamedResponse::default();
        }
    };
    let parsed = usage_from_value(&value, provider);
    let (tool_uses, bound_tokens) = match provider {
        Provider::Anthropic => (
            response_tool_uses_from_value(&value),
            response_bound_tokens_from_value(&value),
        ),
        Provider::Codex => (Vec::new(), Vec::new()),
    };
    NonStreamedResponse {
        usage: parsed.usage,
        model: parsed.model,
        tool_uses,
        bound_tokens,
    }
}

/// The token breakdown of a non-streamed body, parsing it on its own: the tests'
/// entry point. The proxy reads it through [`parse_non_streamed_response`].
#[cfg(test)]
fn usage_from_json(bytes: &[u8], provider: Provider) -> JsonParse {
    let response = parse_non_streamed_response(bytes, provider);
    JsonParse {
        usage: response.usage,
        model: response.model,
    }
}

/// The `tool_use` events of a non-streamed body, parsing it on its own and
/// silently: the tests' entry point.
#[cfg(test)]
fn response_tool_uses_from_json(bytes: &[u8]) -> Vec<crate::session_wire::ToolUseEvent> {
    serde_json::from_slice::<Value>(bytes)
        .map_or_default(|value| response_tool_uses_from_value(&value))
}

/// The bound tokens of a non-streamed body, parsing it on its own and silently:
/// the tests' entry point.
#[cfg(test)]
fn response_bound_tokens_from_json(bytes: &[u8]) -> Vec<MintedToken> {
    serde_json::from_slice::<Value>(bytes)
        .map_or_default(|value| response_bound_tokens_from_value(&value))
}

/// The token breakdown of a parsed non-streamed messages body. No `usage` key
/// is ordinary and yields the default.
fn usage_from_value(value: &Value, provider: Provider) -> JsonParse {
    let model = value
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_string);
    let Some(usage) = value.get("usage") else {
        // Ordinary, and deliberately silent: plenty of 200 bodies carry no usage.
        return JsonParse {
            model,
            ..JsonParse::default()
        };
    };
    match provider {
        Provider::Codex => {
            return JsonParse {
                usage: codex_parsed_usage(usage),
                model,
            };
        }
        Provider::Anthropic => {}
    }
    let output = usage
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let (cache_read, cache_creation, cache_creation_1h) = cache_breakdown(usage);
    JsonParse {
        usage: ParsedUsage {
            input_total: sum_input_tokens(usage),
            output,
            cache_read,
            cache_creation,
            cache_creation_1h,
            reasoning: 0,
        },
        model,
    }
}

/// What [`codex_terminal_usage`] reads off a Codex stream's terminal event: the turn's usage,
/// in the same shape the Anthropic parse produces, and the model that served it.
struct CodexSseParse {
    usage: ParsedUsage,
    model: Option<String>,
}

/// A Codex `usage` object in the ledger's shape. `input_tokens` INCLUDES both cache parts,
/// `input_tokens_details.cached_tokens` (read from the cache) and
/// `input_tokens_details.cache_write_tokens` (written to it), and `output_tokens` includes
/// reasoning, with `output_tokens_details.reasoning_tokens` naming how much (`openai/codex`
/// `codex-api/src/sse/responses.rs:105-160`). The one reader for
/// both places a Codex turn reports its usage, a stream's terminal event
/// ([`codex_terminal_usage`]) and a non-streamed Responses object ([`parse_non_streamed_response`]), so
/// the two cannot disagree.
fn codex_parsed_usage(usage: &Value) -> ParsedUsage {
    let get = |v: &Value, k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
    let details = usage.get("input_tokens_details");
    ParsedUsage {
        input_total: get(usage, "input_tokens"),
        output: get(usage, "output_tokens"),
        cache_read: details.map_or(0, |d| get(d, "cached_tokens")),
        cache_creation: details.map_or(0, |d| get(d, "cache_write_tokens")),
        // `Some(0)`, not `None`: the 1h figure is Anthropic's extended-TTL cache write, and
        // nothing a Codex turn writes is one. `None` means "no split reported" and hands
        // the question to `usage_record`'s fallback, which reads the REQUEST's Anthropic
        // `cache_control` ttl, and that is no evidence about a Codex body.
        cache_creation_1h: Some(0),
        reasoning: usage
            .get("output_tokens_details")
            .map_or(0, |d| get(d, "reasoning_tokens")),
    }
}

/// The response object of a completed or incomplete turn, even when usage is absent.
fn codex_terminal_response(event: &Value) -> Option<&Value> {
    match event.get("type")?.as_str()? {
        "response.completed" | "response.incomplete" => event.get("response"),
        _ => None,
    }
}

/// One Codex SSE event's usage, if it is a terminal event that spent tokens. A Codex Responses
/// turn that runs to its end closes on `response.completed`, or on `response.incomplete` when
/// the backend cut it short, and both carry `response.usage` ([`codex_parsed_usage`];
/// `openai/codex` `codex-api/src/sse/responses.rs:418-440`). A turn that fails closes on
/// `response.failed` instead, which [`parse_sse_usage`] records as a stream error, never as
/// usage. The model is read from the same object: a Codex request body is usually zstd and then
/// cannot be peeked for one, so this is where tcr reads a streamed Codex turn's model.
///
/// `None` for every other event (a delta; `response.created`, which names the model but has no
/// usage yet; `response.failed`, whatever it carries) and for a terminal event whose `usage`
/// is null or absent, so [`parse_sse_usage`] leaves its totals as they were rather than
/// zeroing them.
fn codex_terminal_usage(event: &Value) -> Option<CodexSseParse> {
    let response = codex_terminal_response(event)?;
    let usage = response.get("usage").filter(|u| !u.is_null())?;
    Some(CodexSseParse {
        usage: codex_parsed_usage(usage),
        model: response
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// [`codex_terminal_usage`] over one event's raw bytes, the form the tests hold their fixtures
/// in; `None` for bytes that are not JSON. [`parse_sse_usage`] parses each event once, itself.
#[cfg(test)]
fn codex_usage_from_event(data: &[u8]) -> Option<CodexSseParse> {
    codex_terminal_usage(&serde_json::from_slice(data).ok()?)
}

/// The kind a failed Codex turn is recorded under: the `code` of its error object
/// (`response.error` on a `response.failed` event, the nested `error` on an `error` event),
/// such as `rate_limit_exceeded`, `context_length_exceeded`, `insufficient_quota`,
/// `server_is_overloaded` or `cyber_policy`; `"unknown"` without one, as on the Anthropic arm.
fn codex_error_kind(error: Option<&Value>) -> String {
    error
        .and_then(|e| e.get("code"))
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string()
}

/// Parse the `tool_use` blocks out of a non-streamed messages body — the JSON-path twin of
/// [`parse_sse_usage`]'s block assembly, feeding the same
/// [`crate::session_wire::tool_use_event_from_block`].
///
/// Reads the tree [`parse_non_streamed_response`] already parsed, and returns its own `Vec`
/// beside [`ParsedUsage`] rather than on it, because `ParsedUsage` is `Copy` on purpose (see
/// its doc). A body that carries no `content` array yields no events.
fn response_tool_uses_from_value(value: &Value) -> Vec<crate::session_wire::ToolUseEvent> {
    let Some(blocks) = value.get("content").and_then(Value::as_array) else {
        return Vec::new();
    };
    blocks
        .iter()
        .filter(|b| {
            b.get("type")
                .and_then(Value::as_str)
                .is_some_and(|t| t == "tool_use")
        })
        .take(crate::session_wire::PENDING_TOOL_CAP)
        .filter_map(|b| {
            let id = b.get("id").and_then(Value::as_str)?.to_string();
            Some(crate::session_wire::tool_use_event_from_block(
                id,
                b.get("name").and_then(Value::as_str).map(str::to_string),
                b.get("input"),
            ))
        })
        .collect()
}

/// Parse the account-bound tokens out of a non-streamed messages body — the JSON-path twin of
/// [`parse_sse_usage`]'s `bound_tokens`, feeding
/// [`crate::manager::Manager::record_bound_tokens`].
///
/// Reads the tree [`parse_non_streamed_response`] already parsed.
///
/// The body's own `id` is one of them: a later request may name it as `previous_message_id`,
/// and the thread state behind that id lives on the account that answered — `404 No thread
/// state was found for the requested previous_message_id` on any other.
fn response_bound_tokens_from_value(value: &Value) -> Vec<MintedToken> {
    let mut tokens: Vec<MintedToken> = Vec::new();
    if let Some(id) = value.get("id").and_then(Value::as_str) {
        tokens.push((BoundTokenKind::MessageId, id.to_string()));
    }
    let Some(blocks) = value.get("content").and_then(Value::as_array) else {
        return tokens;
    };
    for block in blocks {
        let field = |name: &str| block.get(name).and_then(Value::as_str).map(str::to_string);
        let minted = match block.get("type").and_then(Value::as_str) {
            Some(SERVER_TOOL_USE_BLOCK_TYPE) => {
                field("id").map(|t| (BoundTokenKind::ServerToolUse, t))
            }
            Some(THINKING_BLOCK_TYPE) => {
                field("signature").map(|t| (BoundTokenKind::ThinkingSignature, t))
            }
            Some(REDACTED_THINKING_BLOCK_TYPE) => {
                field("data").map(|t| (BoundTokenKind::RedactedThinking, t))
            }
            _ => None,
        };
        if let Some(minted) = minted {
            tokens.push(minted);
        }
        if tokens.len() >= crate::session_wire::BOUND_TOKEN_SCAN_CAP {
            break;
        }
    }
    tokens
}

/// Parse the total usage breakdown from an SSE messages stream, plus — out of
/// band, so [`ParsedUsage`] can stay `Copy` — the FIRST in-band `error` event's
/// `error.type`, if any arrived, every COMPLETED `tool_use` block the turn
/// emitted, and every account-bound token it minted.
///
/// The bound tokens are their own value and not part of the tool blocks: none of them is a
/// client-side call the Tools tab has anything to show. What they ARE is the thing that decides
/// which account may serve the REST of this conversation — see [`crate::bound_tokens`]. The
/// message id and a `server_tool_use` id are taken the moment their event arrives, because
/// each is whole there; a stream severed mid-turn has still told us who minted them. A
/// thinking signature arrives as `signature_delta` fragments and is taken at
/// `content_block_stop`, whole or not at all.
///
/// The tool blocks are what the Tools tab's RUNNING NOW section is built from,
/// and the response is the only place they can come from: a Claude Code request
/// body carries each `tool_use` alongside its `tool_result`, so the request-side
/// parse opens and closes a call in the same breath and nothing is ever seen in
/// flight (measured 2026-09-13: 11 live sessions, `running: 0` in all of them,
/// three of them mid-tool). A block is assembled from `content_block_start`
/// (id, name), the `input_json_delta` fragments of its `input` object, and
/// `content_block_stop`, then turned into an event by the same
/// [`crate::session_wire::tool_use_event_from_block`] the request side uses —
/// one head-building rule, two callers.
///
/// `input_total` is taken from `message_start` (base + cache tokens); `output`
/// is the latest cumulative count from `message_delta` (or `message_start` if
/// no delta arrives). Returning the totals — rather than incrementing per event
/// — is what makes the count applied exactly once, never doubled.
/// eventsource-stream reassembles events split across network chunks, so a
/// boundary-split `message_start` is still parsed whole (bug #1 designed out).
/// The cache components come from the same `message_start` usage via the shared
/// [`cache_breakdown`] (R2: identical extraction to the JSON path).
///
/// An Anthropic error envelope can arrive INSIDE a 200 `text/event-stream` body
/// (the same shape this proxy itself synthesizes — see [`error_response`] used
/// as a template, and the `event: error` fixture below) — that is a truncated,
/// failed turn, not usage to add to the quota. First `error` event wins; a
/// second is ignored, so multi-event streams still name their root cause.
///
/// `message_stop` is Anthropic's ONLY marker for "this turn completed" — it
/// does not appear anywhere else in this crate (grepped: zero hits before this
/// function existed). A stream that ends without one — the transport dying
/// mid-body, or the malformed/utf8 `break` above — is exactly as truncated as
/// one that carries an in-band `error` event, so it gets the same treatment:
/// `stream_error` is set, this time to the fixed kind [`TRUNCATED_STREAM_ERROR_KIND`],
/// UNLESS an `error` event already explained the failure (that one names a
/// root cause; this one only names the absence of proof the turn finished).
///
/// The remaining case is a stream that never saw `message_start` — this used
/// to suppress the verdict unconditionally, on the theory that such a stream
/// was "never confirmed to be a Messages turn" (a non-2xx SSE error body, a
/// HEAD response, or a future non-Messages endpoint sharing the content-type).
/// That conflated two shapes that are NOT the same: a stream cut off before
/// its first parseable event (zero events observed at all — `!saw_any_event`)
/// and a stream that genuinely produced OTHER events but none of them
/// `message_start` (`saw_any_event && !saw_message_start`). Only the second
/// one is evidence the stream was never a Messages turn; the first is
/// indistinguishable from a Messages turn severed before `message_start` ever
/// arrived — the single worst case this classifier exists to catch, since it
/// otherwise records nothing at all. So the verdict fires when `message_start`
/// was seen (truncated mid-turn) OR nothing was ever seen (severed before the
/// first event) — and stays silent only when events arrived that affirmatively
/// were not `message_start`.
///
/// `provider` picks the event vocabulary and nothing else: the framing is shared, so a Codex
/// event split across chunks is reassembled exactly like an Anthropic one. A Codex event is
/// parsed once, in its own branch, and never reaches the Anthropic arms, not even one typed
/// `error`, whose shape is OpenAI's. A Codex stream's usage is its terminal
/// `response.completed` / `response.incomplete` event alone, read by [`codex_terminal_usage`],
/// which also yields the model: a Codex request body is usually zstd, and the stream is the
/// place tcr reads a Codex turn's model. A finished output item that is a tool call
/// (`response.output_item.done`, read by [`crate::session_wire::codex_tool_call_from_item`])
/// becomes one of [`SseParse::codex_tool_calls`]. The stream's failures are classified by the
/// same rules as Anthropic's, in Codex's vocabulary. A `response.failed` event is recorded
/// under its `response.error.code` and an `error` event under its nested `error.code`
/// ([`codex_error_kind`]), the first one winning; `response.incomplete` is not a failure here,
/// because the turn's tokens were spent and are counted. A Codex turn ends in
/// `response.completed`, `response.incomplete` or `response.failed`, and a stream that ends
/// without one, including one severed before its first event, gets the same
/// [`TRUNCATED_STREAM_ERROR_KIND`] verdict as an Anthropic stream without `message_stop`. It
/// needs no `message_start` to count as a turn on the Responses endpoint. Auxiliary
/// image, memory and search streams have their own framing; they never synthesize
/// a missing Responses terminator, while explicit error events still count.
/// A Codex quota or rate-limit failure also returns [`SseParse::codex_stream_limit`]
/// so the caller holds that account for subsequent turns without replaying this stream.
async fn parse_sse_usage<S, B, E>(stream: S, provider: Provider, path: &str) -> SseParse
where
    S: futures::Stream<Item = Result<B, E>>,
    B: AsRef<[u8]>,
{
    let events = stream.eventsource();
    futures::pin_mut!(events);

    let mut parsed = ParsedUsage::default();
    // Keep the model attached to retained usage separate from catalogue observations.
    let mut model: Option<String> = None;
    let mut response_model: Option<String> = None;
    let mut stream_error: Option<String> = None;
    let mut codex_stream_limit = None;
    let mut saw_any_event = false;
    let mut saw_message_start = false;
    let mut saw_message_stop = false;
    // A Codex turn's end: `response.completed`, `response.incomplete` or `response.failed`.
    let mut saw_codex_end = false;
    // `tool_use` blocks under construction, keyed by the stream's own block index. A block
    // arrives as a `content_block_start` (id + name), then its `input` object one
    // `input_json_delta` fragment at a time, then `content_block_stop` — so nothing is a
    // complete event until the stop, and only completed blocks are returned.
    let mut open_tool_blocks: std::collections::HashMap<u64, OpenToolBlock> =
        std::collections::HashMap::new();
    let mut tool_uses: Vec<crate::session_wire::ToolUseEvent> = Vec::new();
    let mut codex_tool_calls = Vec::new();
    // Account-bound tokens this turn minted — the message id, every server-tool id, every
    // thinking signature, every redacted-thinking payload. See `crate::bound_tokens`.
    let mut bound_tokens: Vec<MintedToken> = Vec::new();
    // Thinking signatures under construction, keyed by block index: a signature arrives as
    // `signature_delta` fragments that concatenate, so it is complete only at
    // `content_block_stop` — unlike a server-tool id, which is whole the moment its block
    // opens.
    let mut open_signatures: std::collections::HashMap<u64, String> =
        std::collections::HashMap::new();
    while let Some(item) = events.next().await {
        let Ok(event) = item else {
            break; // malformed/utf8/transport error — stop parsing, keep totals
        };
        if event.data.is_empty() {
            continue;
        }
        saw_any_event = true;
        match provider {
            Provider::Codex => {
                let Ok(value) = serde_json::from_str::<Value>(&event.data) else {
                    continue;
                };
                // What the turn spent: only a `response.completed` / `response.incomplete` says.
                if let Some(terminal) = codex_terminal_usage(&value) {
                    parsed = terminal.usage;
                    model = terminal.model;
                }
                if let Some(response) = codex_terminal_response(&value) {
                    response_model = response
                        .get("model")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                }
                // How it ended. First failure wins, as on the Anthropic arm below.
                match value.get("type").and_then(Value::as_str) {
                    Some("response.completed" | "response.incomplete") => saw_codex_end = true,
                    Some("response.failed") => {
                        saw_codex_end = true;
                        if stream_error.is_none() {
                            let error = value.get("response").and_then(|r| r.get("error"));
                            codex_stream_limit = error.and_then(CodexStreamLimit::from_error);
                            stream_error = Some(codex_error_kind(error));
                        }
                    }
                    Some("error") if stream_error.is_none() => {
                        codex_stream_limit =
                            value.get("error").and_then(CodexStreamLimit::from_error);
                        stream_error = Some(codex_error_kind(value.get("error")));
                    }
                    _ => {}
                }
                // A tool call the client runs: a finished output item only, never the
                // `response.output_item.added` that announces one before its arguments are complete.
                if value.get("type").and_then(Value::as_str) == Some("response.output_item.done")
                    && codex_tool_calls.len() < crate::session_wire::PENDING_TOOL_CAP
                    && let Some(call) = value
                        .get("item")
                        .and_then(crate::session_wire::codex_tool_call_from_item)
                {
                    codex_tool_calls.push(call);
                }
                continue;
            }
            Provider::Anthropic => {}
        }
        let Ok(value) = serde_json::from_str::<Value>(&event.data) else {
            continue;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                saw_message_start = true;
                model = value
                    .get("message")
                    .and_then(|m| m.get("model"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                response_model = model.clone();
                // The response's own message id. A later request may name it as
                // `previous_message_id`, and the server-side thread state behind it lives on
                // the account that answered — `404 No thread state was found` anywhere else.
                if let Some(id) = value
                    .get("message")
                    .and_then(|m| m.get("id"))
                    .and_then(Value::as_str)
                {
                    push_bound_token(&mut bound_tokens, BoundTokenKind::MessageId, id.to_string());
                }
                if let Some(usage) = value.get("message").and_then(|m| m.get("usage")) {
                    parsed.input_total = sum_input_tokens(usage);
                    (
                        parsed.cache_read,
                        parsed.cache_creation,
                        parsed.cache_creation_1h,
                    ) = cache_breakdown(usage);
                    if let Some(out) = usage.get("output_tokens").and_then(Value::as_u64) {
                        parsed.output = out;
                    }
                }
            }
            Some("message_delta")
                if let Some(out) = value
                    .get("usage")
                    .and_then(|u| u.get("output_tokens"))
                    .and_then(Value::as_u64) =>
            {
                parsed.output = out;
            }
            Some("message_stop") => {
                saw_message_stop = true;
            }
            Some("content_block_start") => {
                let block = value.get("content_block");
                let block_type = block.and_then(|b| b.get("type")).and_then(Value::as_str);
                let is_tool_use = block_type.is_some_and(|t| t == "tool_use");
                let id = block
                    .and_then(|b| b.get("id"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                match block_type {
                    // A SERVER-side tool call (`--advisor` today; `web_search` is next).
                    // Recorded by id alone: it is Anthropic's own call, nothing the client
                    // runs, and the only thing that matters later is which account minted it.
                    Some(SERVER_TOOL_USE_BLOCK_TYPE) if let Some(id) = id.as_deref() => {
                        push_bound_token(
                            &mut bound_tokens,
                            BoundTokenKind::ServerToolUse,
                            id.to_string(),
                        );
                    }
                    // A reasoning block: open the buffer its `signature_delta`s append to.
                    Some(THINKING_BLOCK_TYPE) if let Some(index) = block_index(&value) => {
                        // A block start may already carry the whole signature (the
                        // non-streamed shape arriving over SSE); the deltas then append to
                        // it rather than to an empty string.
                        let head = block
                            .and_then(|b| b.get("signature"))
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        open_signatures.insert(index, head);
                    }
                    // Already encrypted, and whole at the start — nothing to assemble.
                    Some(REDACTED_THINKING_BLOCK_TYPE)
                        if let Some(data) =
                            block.and_then(|b| b.get("data")).and_then(Value::as_str) =>
                    {
                        push_bound_token(
                            &mut bound_tokens,
                            BoundTokenKind::RedactedThinking,
                            data.to_string(),
                        );
                    }
                    _ => {}
                }
                if let (true, Some(index), Some(id)) = (is_tool_use, block_index(&value), id) {
                    // A stream can only carry so many tool blocks before the cap that bounds
                    // the table itself applies; stop accumulating rather than grow with the
                    // response.
                    if open_tool_blocks.len() < crate::session_wire::PENDING_TOOL_CAP {
                        open_tool_blocks.insert(
                            index,
                            OpenToolBlock {
                                id,
                                name: block
                                    .and_then(|b| b.get("name"))
                                    .and_then(Value::as_str)
                                    .map(str::to_string),
                                input_json: String::new(),
                                over_cap: false,
                            },
                        );
                    }
                }
            }
            Some("content_block_delta") => {
                // A `signature_delta` carries the next fragment of a reasoning block's
                // signature. They concatenate; the whole value is the token.
                let signature_fragment = value
                    .get("delta")
                    .filter(|d| {
                        d.get("type")
                            .and_then(Value::as_str)
                            .is_some_and(|t| t == "signature_delta")
                    })
                    .and_then(|d| d.get("signature"))
                    .and_then(Value::as_str);
                if let (Some(index), Some(fragment)) = (block_index(&value), signature_fragment)
                    && let Some(open) = open_signatures.get_mut(&index)
                {
                    // Bounded like the tool-input buffer beside it: a signature is a few
                    // hundred bytes, so anything past this cap is a malformed or hostile
                    // stream, and a half-signature is worth nothing anyway — drop the
                    // buffer rather than grow with the response.
                    if open.len() + fragment.len() <= SIGNATURE_CAP {
                        open.push_str(fragment);
                    } else {
                        open.clear();
                    }
                }
                let partial = value
                    .get("delta")
                    .filter(|d| {
                        d.get("type")
                            .and_then(Value::as_str)
                            .is_some_and(|t| t == "input_json_delta")
                    })
                    .and_then(|d| d.get("partial_json"))
                    .and_then(Value::as_str);
                if let (Some(index), Some(partial)) = (block_index(&value), partial)
                    && let Some(open) = open_tool_blocks.get_mut(&index)
                {
                    // A `Write` tool's input carries a whole file; only the head of one
                    // field is ever displayed, so an oversized input is dropped rather
                    // than buffered — the call still shows up, by name, with no head.
                    if open.input_json.len() + partial.len() > TOOL_INPUT_JSON_CAP {
                        open.over_cap = true;
                        open.input_json.clear();
                    } else if !open.over_cap {
                        open.input_json.push_str(partial);
                    }
                }
            }
            Some("content_block_stop") => {
                // A reasoning block's signature is complete here and nowhere earlier.
                if let Some(signature) =
                    block_index(&value).and_then(|i| open_signatures.remove(&i))
                    && !signature.is_empty()
                {
                    push_bound_token(
                        &mut bound_tokens,
                        BoundTokenKind::ThinkingSignature,
                        signature,
                    );
                }
                if let Some(open) = block_index(&value).and_then(|i| open_tool_blocks.remove(&i)) {
                    let input = if open.over_cap {
                        None
                    } else {
                        serde_json::from_str::<Value>(&open.input_json).ok()
                    };
                    tool_uses.push(crate::session_wire::tool_use_event_from_block(
                        open.id,
                        open.name,
                        input.as_ref(),
                    ));
                }
            }
            // First `error` event wins — a later one is ignored (guard skips the
            // arm rather than nesting an `if` inside it, per clippy).
            Some("error") if stream_error.is_none() => {
                let kind = value
                    .get("error")
                    .and_then(|e| e.get("type"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string();
                stream_error = Some(kind);
            }
            _ => {}
        }
    }
    // The loop exited — either the upstream closed cleanly after `message_stop`,
    // or it stopped short. No terminator and no more specific error already
    // recorded means the turn's ending was never observed: that IS truncation,
    // for either of two shapes — `message_start` was seen (a confirmed Messages
    // turn that never reached its terminator) or NOTHING was ever seen at all
    // (`!saw_any_event`, severed before the stream produced a single event —
    // indistinguishable from a Messages turn cut off before `message_start`,
    // so it gets the same verdict rather than a silent pass). The one case
    // that stays silent is events having arrived that affirmatively were not
    // `message_start` — that is the actual proof this was never a Messages
    // turn (see the doc comment above). A Codex stream folds in with its own
    // terminator on the Responses path. Auxiliary Codex endpoints do not promise it.
    let saw_turn_end = saw_message_stop || saw_codex_end;
    let confirmed_turn = match provider {
        Provider::Anthropic => saw_message_start,
        Provider::Codex => true,
    };
    let expects_turn_end = match provider {
        Provider::Anthropic => true,
        Provider::Codex => path == crate::provider::CODEX_RESPONSES_PATH,
    };
    if expects_turn_end
        && !saw_turn_end
        && stream_error.is_none()
        && (confirmed_turn || !saw_any_event)
    {
        stream_error = Some(TRUNCATED_STREAM_ERROR_KIND.to_string());
    }
    // Blocks still open here never reached `content_block_stop` — the turn was cut off before
    // the tool call was complete, so the client never ran it and it is not "running".
    SseParse {
        usage: parsed,
        model,
        response_model,
        stream_error,
        tool_uses,
        bound_tokens,
        codex_tool_calls,
        codex_stream_limit,
    }
}

/// Add one account-bound token to a turn's list, deduplicated and capped.
///
/// One place, three callers (the two SSE arms and the message id), because a token added
/// without the cap is an unbounded allocation driven by the upstream's response.
fn push_bound_token(tokens: &mut Vec<MintedToken>, kind: BoundTokenKind, token: String) {
    if tokens.len() >= crate::session_wire::BOUND_TOKEN_SCAN_CAP
        || tokens.iter().any(|(_, seen)| *seen == token)
    {
        return;
    }
    tokens.push((kind, token));
}

/// Say, once per process per field path, that a client sent `previous_message_id` there.
///
/// Two shapes are read on the request path, and a field nobody ever sends is indistinguishable,
/// from the code, from one that is read wrongly. One line the first time each path is seen
/// turns that into a measurement; a line per request would be a flood on a busy proxy. (This
/// line is how the third shape, `diagnostics.previous_message_id`, was found to be live and
/// then found to be no binding at all: see the request peek in `handle`.)
fn note_previous_message_id_path(path: &'static str) {
    static SEEN: std::sync::Mutex<Option<HashSet<&'static str>>> = std::sync::Mutex::new(None);
    let mut seen = match SEEN.lock() {
        Ok(seen) => seen,
        // A poisoned diagnostic latch must never take down a request.
        Err(poisoned) => poisoned.into_inner(),
    };
    if seen.get_or_insert_with(HashSet::new).insert(path) {
        tracing::info!(
            field = path,
            "a client names server-side thread state here; requests carrying it are held to \
             the account that holds it while that account can serve"
        );
    }
}

/// Most bytes one reasoning-block signature may occupy while being assembled. A real signature
/// is a few hundred bytes; this is slack, not a protocol limit, and it bounds what a malformed
/// stream of `signature_delta`s can make the parser hold.
const SIGNATURE_CAP: usize = 16 * 1024;

/// What one pass of [`parse_sse_usage`] learned about a streamed turn.
///
/// A struct rather than a tuple because `bound_tokens` is the value that decides ROUTING, and a
/// caller that silently bound it to the wrong position would pin conversations to the wrong
/// account — the exact failure this whole feature exists to prevent. Named fields make that
/// misbinding impossible to write.
struct SseParse {
    usage: ParsedUsage,
    /// The model attached to retained usage: a metered terminal for `codex`.
    model: Option<String>,
    /// The response's own identity, even on an unmetered terminal. Catalogue observations
    /// never replace the model attached to usage retained from an earlier terminal.
    response_model: Option<String>,
    /// The FIRST in-band failure's kind (an Anthropic `error` event's `error.type`; a Codex
    /// `response.failed` or `error` event's code, see [`codex_error_kind`]), or the fixed
    /// [`TRUNCATED_STREAM_ERROR_KIND`] when the turn's end (`message_stop`; a Codex terminal
    /// event) was never observed.
    stream_error: Option<String>,
    /// Positive quota/rate-limit advice from the first Codex error event.
    /// Other stream errors remain observational and never change routing.
    codex_stream_limit: Option<CodexStreamLimit>,
    /// Every COMPLETED client-side `tool_use` block — what the Tools tab's RUNNING NOW is built
    /// from.
    tool_uses: Vec<crate::session_wire::ToolUseEvent>,
    /// Every account-bound token the turn minted — see [`crate::bound_tokens`].
    bound_tokens: Vec<MintedToken>,
    /// Every tool call a Codex stream's finished output items asked for, in stream order, at
    /// most `PENDING_TOOL_CAP` of them; always empty on an Anthropic stream, whose calls are
    /// `tool_uses`.
    codex_tool_calls: Vec<crate::session_wire::CodexToolCall>,
}

/// Codex's failed-response reader recognizes quota codes and transient rate limits
/// even when the HTTP response was successful. Read only their reset/retry advice;
/// context, prompt, policy and overload errors do not condemn an account's quota.
struct CodexStreamLimit {
    headers: HeaderMap,
    refusal: Option<crate::quota::CodexQuotaRefusal>,
    hold_seconds: i64,
}

impl CodexStreamLimit {
    fn from_error(error: &Value) -> Option<Self> {
        let mut headers = HeaderMap::new();
        if let Some(values) = error.get("headers").and_then(Value::as_object) {
            for (name, value) in values {
                if (name.eq_ignore_ascii_case("retry-after")
                    || name.to_ascii_lowercase().starts_with("x-codex-"))
                    && let (Ok(name), Some(Ok(value))) = (
                        HeaderName::from_bytes(name.as_bytes()),
                        value.as_str().map(HeaderValue::from_str),
                    )
                {
                    headers.insert(name, value);
                }
            }
        }
        let now = OffsetDateTime::now_utc();
        let refusal = crate::quota::CodexQuotaRefusal::from_error(&headers, Some(error), now);
        if refusal.is_none()
            && !matches!(
                error.get("code").and_then(Value::as_str),
                Some("rate_limit_exceeded" | "slow_down")
            )
        {
            return None;
        }
        let hold_seconds = refusal
            .as_ref()
            .and_then(|refusal| refusal.reset)
            .map_or_else(
                || {
                    parse_retry_after(&headers).unwrap_or_else(|| {
                        if refusal.is_some() {
                            60
                        } else {
                            NO_GUIDANCE_HOLD_SECS
                                + i64::from(now.nanosecond()) % (NO_GUIDANCE_JITTER_MAX_SECS + 1)
                        }
                    })
                },
                |reset| reset.unix_timestamp().saturating_sub(now.unix_timestamp()),
            );
        Some(Self {
            headers,
            refusal,
            hold_seconds,
        })
    }
}

/// One `tool_use` content block being assembled across SSE events — see [`parse_sse_usage`].
struct OpenToolBlock {
    id: String,
    name: Option<String>,
    input_json: String,
    /// The `input` object exceeded [`TOOL_INPUT_JSON_CAP`], so it was dropped: the event is
    /// still emitted (the call IS running), just without a command head.
    over_cap: bool,
}

/// Most bytes of one streamed `tool_use` input object [`parse_sse_usage`] will buffer. Only a
/// 120-character head is ever displayed, so this is display slack, not a protocol limit — and
/// it bounds what a `Write` of a large file can make the parser hold.
const TOOL_INPUT_JSON_CAP: usize = 64 * 1024;

/// An SSE content-block event's `index`, which keys every block-scoped event to its block.
fn block_index(event: &Value) -> Option<u64> {
    event.get("index").and_then(Value::as_u64)
}

/// A JSON error response in Anthropic's error envelope.
///
/// Every proxy-GENERATED refusal is built here, so this is the one place that
/// can log all of them — the second blind spot measured 2026-09-07: the
/// fleet-exhausted message never appeared in the log at all (`grep -cE
/// 'quota exhausted on all|accounts exhausted'` -> `0` for a day the fleet sat
/// at 0.95+ on `7d_oi`), because the only line near it (`"returning exhausted
/// 429 to client"`, see [`exhausted_response`]) never carried the message
/// text itself. Callers that already log their own richer context (account
/// count considered, the gate that bound the hint, and so on) still hit this
/// line too — a second, low-detail line is cheap; a refusal with NO line at
/// all is the bug this exists to close.
///
/// The one exception is [`quiet_error_response`], for an answer that is
/// EXPECTED on a healthy path; its caller logs it at the level it deserves.
fn error_response(
    status: StatusCode,
    err_type: &str,
    message: &str,
    retry_after: Option<i64>,
) -> Response {
    client_error::warn_refusal(status, err_type, message);
    quiet_error_response(status, err_type, message, retry_after)
}

/// [`error_response`]'s envelope without its warn line. Only for an answer a
/// healthy session produces as a matter of course, where a warn per occurrence
/// would read as a fault: the Codex 426 ([`upgrade_required_response`]), one
/// per Codex session. The caller logs it itself, so the refusal is never
/// line-less (the bug [`error_response`] exists to close).
fn quiet_error_response(
    status: StatusCode,
    err_type: &str,
    message: &str,
    retry_after: Option<i64>,
) -> Response {
    let payload = serde_json::json!({
        "type": "error",
        "error": { "type": err_type, "message": message },
    });
    let mut response = Response::new(Body::from(payload.to_string()));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    if let Some(seconds) = retry_after
        && let Ok(value) = HeaderValue::from_str(&seconds.to_string())
    {
        response.headers_mut().insert("retry-after", value);
    }
    response
}

// The lint allow sits ABOVE the doc block, not between it and the signature:
// `tests/peer_egress.rs` (`the_dry_fleet_terminals_doc_does_not_call_the_fallback_arm_dead`)
// reads the run of `///` lines directly above `async fn exhausted_or_fallback(`.
#[expect(
    clippy::too_many_arguments,
    reason = "the request state that both the 429 and the fallback ask are built from"
)]
/// The dry-fleet terminal: consult the fallback ladder once, then answer with
/// exactly what this handler answered before it existed.
///
/// Placed here rather than earlier in the arm because everything local is
/// cheaper than anything remote: the transient-park soft wait and the
/// revalidation serve both run first, so a recovery this machine could have
/// made on its own is never pre-empted by a remote one.
///
/// **The provider arm is LIVE, and every claim about this handler has to be
/// read that way.** This doc used to say the opposite: that
/// [`fallback::configured_provider`] answers `None` on this branch, so the
/// whole terminal was the old exhaustion answer and nothing past it could run.
/// The boot sequence installs one (`fallback::install_peer_lease_provider`,
/// from the peers file, as soon as a row says this node may borrow) and
/// `src/server.rs` then asks for it back and `expect`s it. So a dry fleet here
/// really does reach another Mac's account, and reasoning that treats the code
/// below as dead reaches the wrong answer.
///
/// [`exhausted_response`] is still what a request gets when there is no
/// provider, when the guards below refuse one, or when the provider declines:
/// that half is unchanged.
///
/// A **reserved group** never reaches a provider. A reserved group means "one of
/// these accounts or nothing", and a peer's account is not one of them; the
/// third spill door the revalidation serve had to be gated against is the same
/// door here.
///
/// A **hard account lock** never reaches one either, for the same sentence with
/// a different subject: `lock_account` means this one account or nothing, and a
/// peer's account is not it. See the check at the top of the body.
///
/// A request with **account-bound history** (`bound_to`, see
/// [`crate::bound_tokens`]) never reaches one, and for a sharper reason: a
/// peer's account cannot read that history, so the provider would answer the
/// same 400 or 404 the bench exists to prevent. Its 429 is timed on the bound
/// account alone.
///
/// The other terminal in this handler, the attempt budget running out while
/// still rotating, at the tail of the loop, is deliberately NOT routed through
/// this function yet. It is a different question ("we walked the fleet and ran
/// out of tries") and whether a lease should be spent there is the lease
/// phase's call, with a re-send hazard to weigh rather than a line to copy.
///
/// A **Codex** request never reaches one either: the seam builds no `Ask` for a
/// Codex path (`peer::serve::serve_is_allowed_for_path`), because Codex is not
/// lent over the mesh in this version. Its 429 is Codex-shaped; see
/// [`exhausted_response`].
async fn exhausted_or_fallback(
    manager: &Manager,
    now: OffsetDateTime,
    provider_account_count: usize,
    is_fable: bool,
    provider: Provider,
    strict_group: Option<&str>,
    bound_to: Option<usize>,
    ask: Option<&fallback::Ask<'_>>,
) -> Response {
    // **A HARD ACCOUNT LOCK NEVER REACHES A PROVIDER.** `lock_account` is the
    // operator saying this fleet is one account for as long as it is set, so a
    // request it could not serve has no failover at all: not another local
    // account (which is why `select_with_group` returned `None` in the first
    // place), not the revalidation serve, and least of all a peer's credential
    // on another Mac. This arm was reached by every caller when the seam landed
    // and the provider read no lock, so a pinned request was served on a peer's
    // account the moment its own account 429'd.
    //
    // Checked HERE rather than at the three call sites: a terminal somebody
    // adds later gets the rule for free, and the rule has one spelling.
    let locked = manager.locked_account_for(provider).is_some();
    // `None` is a path this seam refused to build an `Ask` for at all, a
    // client-credential path, which no provider may be offered. See the arm at
    // `:2352`.
    if let (Some(ask), None, false, None) = (ask, strict_group, locked, bound_to)
        && let Some(provider) = fallback::configured_provider()
        && let Some(response) = provider.try_serve(ask).await
    {
        tracing::info!(
            provider = provider.name(),
            "no local account could serve this request, served through a fallback provider"
        );
        return response;
    }
    exhausted_response(
        manager,
        now,
        provider_account_count,
        is_fable,
        provider,
        strict_group,
        bound_to,
    )
}

/// 429 with a fleet-wide `retry-after` hint when no account is currently usable.
/// `is_fable` scopes the hint to the request's model class so a Fable request is
/// told the true Fable-weekly recovery instant (see [`Manager::exhaustion_hint`]).
/// `strict_group` names the RESERVED group this request asked for, when it asked
/// for one. It changes both halves of the answer, because for a strict request
/// the fleet-wide story is a lie in both:
///
/// - the hint is sized over the group's members only, since an unrelated
///   account un-gating sooner tells this client nothing about when it can be
///   served; and
/// - the message names the group instead of claiming every account is spent,
///   which would send an operator to look at a fleet that is mostly idle.
///
/// The response speaks the client's language, not only ours. Claude Code labels
/// any 429 that carries no `anthropic-ratelimit-unified-status` header "Server
/// is temporarily limiting requests (not your usage limit)" — the opposite of
/// the truth when every account is inside a quota window. So when the gate that
/// times `retry-after` IS a quota window, the 429 also carries the unified
/// `status: rejected`, the `reset` epoch and the `representative-claim` the
/// client maps to its own limit types, and the client renders a usage limit with
/// the real reset instant. A hold or an unknown gate stays a plain 429: an
/// upstream `retry-after` is not the user's usage limit, and claiming one would
/// be the same lie in the other direction. So does a 429 for held history
/// (`bound_to`), whatever gates its one account: see the comment at the claim.
///
/// The `retry-after` figure is computed live from when an account actually
/// frees, never a constant — the only fixed number here is the 60 the hint falls
/// back to when NO account advertises a reset, and the message says so rather
/// than printing it as if it had been measured. What goes on the HEADER is that
/// figure capped at [`CLIENT_RETRY_AFTER_MAX_SECS`] (see
/// [`advertised_retry_after`]): the client refuses a longer one outright. The
/// body keeps the uncapped figure and the real instant for a human reader.
///
/// `provider` is the request's own. The hint folds only that provider's accounts
/// ([`Manager::exhaustion_hint`]), since no other account could have served the
/// request, and a **Codex** client gets a Codex body instead of the Anthropic
/// envelope, which it would print as an opaque error. Which body follows the
/// Anthropic arm's principle, that a quota is the user's limit and a hold is not:
/// a pool out on a quota ([`codex_limit_is_quota`], which counts the backend's
/// refusal as one) gets [`codex_exhausted_body`], which codex renders as a
/// usage-limit line and which ends its turn, and anything else gets
/// [`codex_pool_unavailable_body`], a plain 429 codex retries after the
/// `retry-after`. Either way that header is the same capped one, so the Anthropic
/// half below is untouched. The usage-limit body's `resets_in_seconds` is the same
/// live figure the Anthropic message prints, uncapped, and its `resets_at`, the
/// instant that figure ends (`now` plus it), is there only when some Codex account
/// reports when it frees: the 60-second fallback is no time, and codex would print
/// it as one.
///
/// `provider_account_count` is how many accounts the request's provider has
/// ([`Manager::provider_account_count`]), the only accounts that could have
/// served it, so the Anthropic message's "All N accounts" never counts a Codex
/// account. A Codex pool with no account that can become usable by waiting gets a
/// non-retryable 400 and a configuration/login hint. A missing pool uses
/// [`codex_no_account_body`]; disabled, parked and dead rows get a repair hint.
/// Neither answer carries `retry-after`, because waiting changes nothing.
fn exhausted_response(
    manager: &Manager,
    now: OffsetDateTime,
    provider_account_count: usize,
    is_fable: bool,
    provider: Provider,
    strict_group: Option<&str>,
    bound_to: Option<usize>,
) -> Response {
    let now_ms = (now.unix_timestamp_nanos() / 1_000_000) as i64;
    match provider {
        Provider::Codex if !manager.codex_account_can_serve_eventually(bound_to) => {
            tracing::warn!(
                provider = provider.as_str(),
                "no Codex account can serve by waiting; enable, unpark or repair a login, or add one with `tcr login --codex`"
            );
            manager.note_codex_refusal(CodexRefusalKind::NoCodexAccount, now_ms);
            return codex_error_response(
                StatusCode::BAD_REQUEST,
                if provider_account_count == 0 {
                    codex_no_account_body()
                } else {
                    codex_error_body(CodexRefusalKind::NoCodexAccount,
                        "No usable Codex account is configured in tcr. Enable, unpark or unreserve an account, clear an account lock or unsupported strict peer exit, or repair its login with `tcr login --codex`.").to_string()
                },
                None,
            );
        }
        Provider::Codex | Provider::Anthropic => {}
    }
    // Bound history is the narrowest scope there is — one account — so it wins over a group.
    let hint = match bound_to {
        Some(idx) => manager.exhaustion_hint_for_account(now, is_fable, idx),
        None => manager.exhaustion_hint(now, is_fable, provider, strict_group),
    };
    let retry_after = hint.retry_after;
    let advertised_retry_after = advertised_retry_after(&hint);
    match provider {
        Provider::Codex => {
            let usage_limit = codex_limit_is_quota(&hint);
            tracing::warn!(
                provider = provider.as_str(),
                retry_after,
                advertised_retry_after,
                binding = ?hint.binding,
                usage_limit,
                group = strict_group.unwrap_or("-"),
                "returning exhausted 429 to client"
            );
            // `retry_after` is at least 1 by construction (`Manager::exhaustion_hint`).
            let real_wait_secs = u64::try_from(retry_after).unwrap_or(1);
            let (body, error_type) = if usage_limit {
                (
                    codex_exhausted_body(now, real_wait_secs, hint.free_at.is_some()),
                    CodexRefusalKind::UsageLimitReached,
                )
            } else {
                (
                    codex_pool_unavailable_body(real_wait_secs, hint.free_at.is_some()),
                    CodexRefusalKind::PoolUnavailable,
                )
            };
            manager.note_codex_refusal(error_type, now_ms);
            return codex_error_response(
                StatusCode::TOO_MANY_REQUESTS,
                body,
                Some(advertised_retry_after),
            );
        }
        Provider::Anthropic => {}
    }
    let causes = describe_gates(&hint.gated);
    let when = match hint.free_at {
        Some(at) => format!(
            "Retry in {retry_after}s (soonest account frees at {}).",
            at_utc(at, now)
        ),
        None => format!("No account reports a reset time; retry in {retry_after}s."),
    };
    // Named in the LOG only: the body can travel to a borrowing peer, and an account's name
    // is this operator's, not the client's.
    let bound_name = bound_to.and_then(|idx| manager.account_name(idx));
    let detail = match (bound_to, strict_group) {
        (Some(_), _) => format!(
            "This conversation's history is held on the account that produced it, which is \
             briefly unavailable{causes}; it moves to another account only if this one stays \
             out. {when}"
        ),
        (None, Some(group)) => format!(
            "No account in reserved group '{group}' is available, and a reserved \
             group never serves from outside itself{causes}. {when}"
        ),
        // A pool that holds Codex accounts and no Claude one: "All 0 accounts exhausted" would
        // send its reader to look for Claude accounts that were never there.
        (None, None)
            if provider_account_count == 0
                && manager.provider_account_count(Provider::Codex) > 0 =>
        {
            "No Claude account is configured in tcr; run `tcr login` to add one.".to_string()
        }
        (None, None) => format!("All {provider_account_count} accounts exhausted{causes}. {when}"),
    };
    tracing::warn!(
        account_count = provider_account_count,
        retry_after,
        advertised_retry_after,
        binding = ?hint.binding,
        group = strict_group.unwrap_or("-"),
        bound_to = bound_name.as_deref().unwrap_or("-"),
        "returning exhausted 429 to client"
    );
    let mut response =
        ClientError::new(StatusCode::TOO_MANY_REQUESTS, ErrorKind::RateLimit, detail)
            .retry_after(advertised_retry_after)
            .respond(provider);
    // Held history never carries a claim. The held account's window is not this user's limit:
    // an account that is really out releases the conversation to the fleet
    // (`Manager::bound_account_holds`), so a 429 that reaches here for held history is a wait
    // on one account, and a claim would have Claude Code show "You've hit your session limit"
    // and stop, where a plain 429 is retried by the client on its own.
    let claim = match bound_to {
        Some(_) => None,
        None => hint.binding.and_then(unified_claim_for),
    };
    if let (Some(claim), Some(at)) = (claim, hint.free_at) {
        let headers = response.headers_mut();
        headers.insert(
            "anthropic-ratelimit-unified-status",
            HeaderValue::from_static("rejected"),
        );
        headers.insert(
            "anthropic-ratelimit-unified-representative-claim",
            HeaderValue::from_static(claim),
        );
        if let Ok(value) = HeaderValue::from_str(&at.unix_timestamp().to_string()) {
            headers.insert("anthropic-ratelimit-unified-reset", value);
        }
    }
    response
}

/// The `retry-after` seconds an exhausted 429 puts on the wire.
///
/// A MEASURED wait (`free_at` is `Some`) is capped at
/// [`CLIENT_RETRY_AFTER_MAX_SECS`], because the client refuses anything longer.
/// The unknown-reset sentinel (`free_at` is `None`, `retry_after` is `60`) passes
/// through untouched: 60 is exactly the client's accepted ceiling, and keeping it
/// the only way a `60` reaches the wire is what keeps it distinguishable from a
/// capped measurement.
fn advertised_retry_after(hint: &ExhaustionHint) -> i64 {
    match hint.free_at {
        Some(_) => hint.retry_after.min(CLIENT_RETRY_AFTER_MAX_SECS),
        None => hint.retry_after,
    }
}

/// The `anthropic-ratelimit-unified-representative-claim` value for a gate, or
/// `None` for a gate that is not a quota window. The values are the ones the
/// Claude Code client maps to its limit types (`five_hour`, `seven_day`); the
/// Fable weekly is reported as the plain weekly claim because the client has no
/// Fable-specific one and a weekly reset is what it is.
const fn unified_claim_for(reason: GateReason) -> Option<&'static str> {
    match reason {
        GateReason::FiveHour => Some("five_hour"),
        GateReason::SevenDay | GateReason::FableWeekly => Some("seven_day"),
        GateReason::Ok
        | GateReason::Hold
        | GateReason::Refused
        | GateReason::Standard
        | GateReason::Login
        | GateReason::Rejected
        | GateReason::Disabled
        | GateReason::Reserved
        | GateReason::Parked => None,
    }
}

/// Where a released conversation goes first when its model's thinking survives only inside the
/// bound account's organization ([`crate::model::thinking_is_org_bound`]): `Some(idx)` names a
/// same-organization sibling of `bound_idx` that this request may use, and the caller serves on
/// it without selecting again; `None` leaves the pick to the ordinary rotation, unchanged.
///
/// `select` is the request's own [`Manager::select_with_group`] call with `tried` left open,
/// called once with the request's `tried` plus every account outside the bound account's
/// organization ([`Manager::outside_org_of`], which also holds `bound_idx`). So every rule of
/// the ordinary pick (groups, reservations, pacing, the control account, the session's pin)
/// still applies, only to the siblings. That call mutates nothing when it answers `None`; when
/// it answers `Some` it does what any selection does (the LRU stamp, the session's pin or
/// divert record), which is why the caller passes the index on as `next_idx` and the loop does
/// not select a second time.
///
/// Logs one line for every release of an org-bound model's conversation, `same_org` or
/// `cross_org`; `cross_org` means the API will drop the conversation's earlier thinking. Other
/// models log nothing here and are never steered.
fn same_org_release(
    manager: &Manager,
    bound_idx: usize,
    tried: &HashSet<usize>,
    model: Option<&str>,
    select: impl FnOnce(&HashSet<usize>) -> Option<usize>,
) -> Option<usize> {
    let model = model.filter(|m| crate::model::thinking_is_org_bound(m))?;
    let mut narrowed = manager.outside_org_of(bound_idx);
    narrowed.extend(tried.iter().copied());
    let picked = select(&narrowed);
    tracing::info!(
        outcome = if picked.is_some() {
            "same_org"
        } else {
            "cross_org"
        },
        model,
        from = manager.account_name(bound_idx).as_deref().unwrap_or("?"),
        "releasing a conversation whose thinking is bound to its organization"
    );
    picked
}

/// How long the client is asked to wait before retrying [`switch_warning`]'s 429.
///
/// Long enough that the warning is on screen for a moment before the retry clears it, short
/// enough that a switch, which happens once per conversation per account running out, costs
/// the user a few seconds and no action.
const SWITCH_WARNING_RETRY_AFTER_SECS: i64 = 3;

/// The one 429 a held conversation gets before tcr moves it to another account, or `None`
/// when it should just move.
///
/// A move costs the conversation its prompt cache, and before this it happened silently; the
/// operator asked to be told first (2026-09-24). The 429 is PLAIN, with no unified quota
/// headers, because only a plain one shows this text: measured against Claude Code 2.1.281, a
/// plain 429 prints "API Error: Server is temporarily limiting requests (not your usage
/// limit) · <this message>" and is retried by the client on its own, while one carrying a
/// claim prints "You've hit your session limit" and drops the text. The retry is the request
/// that moves ([`Manager::first_switch_warning`] answers no the second time, and
/// [`Manager::switch_warned`] releases it even if the account crept back meanwhile).
///
/// `None`, and the request moves silently as it did before, in three cases:
/// - this session was already told about this account;
/// - there is no session key to remember the warning by, so warning would warn the retry too,
///   forever;
/// - only this request's MODEL CLASS is out (a Fable request on an account out of its Fable
///   weekly window). Then just this one request diverts and the conversation stays where it
///   is, so there is no move to announce, and announcing one would spend the session's one
///   warning on a move that never happens.
///
/// Names no account, because the body can travel to a borrowing peer.
fn switch_warning(
    manager: &Manager,
    provider: Provider,
    session_key: Option<u64>,
    bound_idx: usize,
    now: OffsetDateTime,
    is_fable: bool,
    group: Option<&str>,
) -> Option<Response> {
    let key = session_key?;
    if is_fable
        && manager.bound_account_holds(bound_idx, now, false, group, manager.warm_window(Some(key)))
    {
        return None;
    }
    if !manager.first_switch_warning(key, bound_idx, crate::now_ms()) {
        return None;
    }
    let hint = manager.exhaustion_hint_for_account(now, is_fable, bound_idx);
    // `binding` is only the gate that TIMES a wait. A terminal gate (rejected, disabled, a dead
    // login) has no clear instant of its own, so it leaves `binding` `None`, an Anthropic
    // `rejected` included. A rejected CODEX account is the one exception, through the
    // all-rejected fold (`Manager::exhaustion_hint`): it comes back `Some(Rejected)` with a
    // `free_at`, the latest live reset among its windows at or over threshold, else its hold's
    // expiry, and stays `None` only when neither is known. Over one account, `gated` holds that
    // account's own reason either way.
    let why = hint
        .binding
        .or_else(|| hint.gated.keys().next().copied())
        .map_or("unavailable", gate_phrase);
    let until = hint
        .free_at
        .map_or_default(|at| format!(" until {}", at_utc(at, now)));
    tracing::info!(
        account = manager.account_name(bound_idx).as_deref().unwrap_or("?"),
        binding = ?hint.binding,
        retry_after = SWITCH_WARNING_RETRY_AFTER_SECS,
        "held conversation's account is out; warning the client once before moving it"
    );
    Some(
        ClientError::new(
            StatusCode::TOO_MANY_REQUESTS,
            ErrorKind::RateLimit,
            format!(
                "tcr: the account holding this conversation is out ({why}{until}); the retry moves \
                 it to another account, where its prompt cache starts cold."
            ),
        )
        .retry_after(SWITCH_WARNING_RETRY_AFTER_SECS)
        .respond(provider),
    )
}

/// `HH:MM:SS UTC` for a message a human reads next to a seconds count, with the date in front
/// once the instant is a day or more after `now`.
///
/// It was the time alone, which reads as today: the live log of 2026-09-24 carries "Retry in
/// 260209s (soonest account frees at 13:00:00 UTC)" for a weekly window three days out.
pub fn at_utc(at: OffsetDateTime, now: OffsetDateTime) -> String {
    let clock = format!("{:02}:{:02}:{:02} UTC", at.hour(), at.minute(), at.second());
    if at - now < time::Duration::DAY {
        clock
    } else {
        format!("{} {clock}", at.date())
    }
}

/// Whether a 502 is the honest verdict: at least one attempt failed in transport
/// AND no attempt ever came back with an upstream HTTP status. Every attempt that
/// gets as far as the send either fails in transport or yields a response, so
/// `upstream_responses == 0` is exactly "transport failure accounts for all of
/// them" — the only state `bad_gateway`'s message describes truthfully.
///
/// A 429, a 5xx, even a 401 is an upstream ANSWER: it proves the network path
/// works and that the right verdict for this request is quota exhaustion (or a
/// forwarded status), never "upstream unreachable". Keeping the two counts apart
/// is what lets one blip rotate away instead of collapsing the recovery ladder.
const fn every_attempt_transport_failed(
    transport_failures: usize,
    upstream_responses: usize,
) -> bool {
    transport_failures > 0 && upstream_responses == 0
}

/// 503 or 502 when every attempt hit a transport failure (upstream unreachable).
/// Gated by [`every_attempt_transport_failed`]; `unknown_outcome` (set by
/// `unknown_outcome_transport_failure` in [`handle`]) picks which of the two
/// shapes is honest:
///
/// - `false`: every attempt died at CONNECT — nothing ever left this box, so a
///   client resending the request costs nothing extra it has not already paid.
///   `503` + `retry-after` + `x-should-retry: true`.
/// - `true`: at least one attempt got PAST connect before failing — a
///   connection existed and the request may have already reached the upstream,
///   so its effect there is unknown and a blind client retry could double-send
///   a POST. `502` + `x-should-retry: false`; main previously answered this
///   case identically to the safe one, which is the gap
///   a fork-contributed review named as worth fixing (network-recovery, A-05,
///   minimal version).
///
/// `retry-after` reuses [`OFFLINE_RETRY_AFTER_SECS`] rather than a new constant:
/// both are "a network hiccup, not a durable fault, come back in a few
/// seconds", and that number is already the one a live measurement
/// (2026-08-10, see its doc-comment) established for exactly that recommendation.
fn bad_gateway(provider: Provider, transport_failures: usize, unknown_outcome: bool) -> Response {
    if unknown_outcome {
        tracing::warn!(
            transport_failures,
            "returning 502 to client — every attempt failed in transport and at least one \
             got past the connect phase, so its outcome upstream is unknown; telling the \
             client not to retry automatically"
        );
        ClientError::new(
            StatusCode::BAD_GATEWAY,
            ErrorKind::Proxy,
            format!(
                "Upstream unreachable: all {transport_failures} attempt(s) failed in transport, \
                 and at least one may have reached the upstream before failing. Its outcome is \
                 unknown — do not retry this request automatically."
            ),
        )
        .should_retry(false)
        .respond(provider)
    } else {
        tracing::warn!(
            transport_failures,
            retry_after = OFFLINE_RETRY_AFTER_SECS,
            "returning 503 to client — every attempt failed at the connect phase; nothing \
             reached an upstream, so retrying is safe"
        );
        ClientError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorKind::Proxy,
            format!(
                "Upstream unreachable: all {transport_failures} attempt(s) failed before a \
                 connection was made. Safe to retry in {OFFLINE_RETRY_AFTER_SECS}s."
            ),
        )
        .retry_after(OFFLINE_RETRY_AFTER_SECS)
        .should_retry(true)
        .respond(provider)
    }
}

/// `503 + Retry-After` when name resolution kept failing: this machine is off the
/// network, which is recoverable and says nothing about any account.
///
/// Deliberately not [`bad_gateway`]. A 502 asserts "every attempt failed in
/// transport, none reached an upstream" — true, but it points the client at the
/// upstream when the fault is local, and it carries no `retry-after`, so a client
/// has no guidance beyond "give up". The account pool is NOT rotated on this path
/// and the session keeps its pin.
fn offline_unavailable(provider: Provider, dns_failures: usize) -> Response {
    tracing::warn!(
        dns_failures,
        retry_after = OFFLINE_RETRY_AFTER_SECS,
        "returning 503 to client — name resolution is failing, this machine is offline; \
         the account pool was NOT rotated"
    );
    ClientError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        ErrorKind::Proxy,
        format!(
            "Name resolution failed on {dns_failures} attempt(s) — this machine appears to be \
             offline. No account was rotated. Retry in {OFFLINE_RETRY_AFTER_SECS}s."
        ),
    )
    .retry_after(OFFLINE_RETRY_AFTER_SECS)
    .respond(provider)
}

/// The answer when every upstream answer this request got was a 403 refusing OUR pooled
/// credential — see the 403 branch above and its gate,
/// `refused_403.len() == upstream_responses`, in the `None`-from-`select` arm.
///
/// Not a 429: waiting will not help, so a `retry-after` would be a lie. Not the
/// bare 403 forwarded — the client never saw the credential upstream refused and
/// re-authenticating THEIR session cannot fix an account problem that is entirely
/// ours. `proxy_error`, naming the refused account(s), so whoever reads the log
/// knows which credential(s) need attention (most likely re-login) rather than
/// guessing from a blank error.
///
/// The status is the one each client handles without harm, so the two providers differ:
///
/// * **A Claude client gets a 502**, as it always has.
/// * **A Codex client gets a 403**, in codex's own `{"error": {...}}` envelope, its message one
///   sentence per refused account, in the order they were refused, each naming the command that
///   signs that account in again. Read against openai/codex f82e365: a 401 is the one status
///   that starts codex's auth recovery (`model-provider/src/provider.rs:172-177`), which re-reads
///   codex's own `auth.json` and then refreshes its token, rewriting that file, so a 401 would
///   spend codex's own login on a refusal that is tcr's. A 403 leaves that login alone: no
///   reload, no refresh, no write or removal of the file, no sign-out and no login prompt. Its
///   transport does not resend a 403 either, where it resends a 5xx up to four times
///   (`retry_5xx`, `model-provider-info/src/lib.rs:454-460`) before the turn's own retries begin.
///   The turn retries any unexpected status up to five times (`core/src/responses_retry.rs`),
///   and each "Reconnecting…" line it prints carries this message.
///
/// Either way the refused account is then held for [`crate::manager::REFUSED_HOLD_SECONDS`]
/// (the 403 branch's `mark_refused`), so this answer reaches a client at most once per hold.
/// While every available Codex account remains refused, later requests keep receiving 403
/// without retry guidance. Anthropic retains its exhaustion 429 and bounded soft wait.
fn credential_refused_response(
    provider: Provider,
    refused: &[(usize, Option<String>)],
) -> Response {
    match provider {
        Provider::Codex => {
            return codex_credential_refused_response(refused);
        }
        Provider::Anthropic => {}
    }
    let account_list: Vec<String> = refused
        .iter()
        .map(|(idx, name)| format!("{} (index {idx})", name.as_deref().unwrap_or("<unnamed>")))
        .collect();
    tracing::warn!(
        refused_accounts = account_list.join(", "),
        "returning 502 to client — every account tried was refused with a 403; \
         re-login is likely required"
    );
    ClientError::new(
        StatusCode::BAD_GATEWAY,
        ErrorKind::Proxy,
        format!(
            "Account credential(s) refused by upstream: {}. Re-login to the account \
             (`tcr login` or the TcrBar UI) — retrying will not help.",
            account_list.join(", ")
        ),
    )
    .respond(provider)
}

/// While every eligible account remains refused, preserve the access-denied status.
/// A workspace deactivation is not repaired by refreshing its otherwise valid login.
fn codex_refused_pool_response() -> Response {
    codex_error_response(
        StatusCode::FORBIDDEN,
        codex_error_body(CodexRefusalKind::CredentialRefused,
            "ChatGPT refused access for the configured Codex accounts. Check account and workspace access before retrying.").to_string(),
        None,
    )
}

/// [`credential_refused_response`] for a Codex client: a 403 with `content-type:
/// application/json`, no `retry-after`, and codex's envelope. The warn line names the command
/// for each refused account, so the fix reaches the log even when codex shows no body.
fn codex_credential_refused_response(refused: &[(usize, Option<String>)]) -> Response {
    let names: Vec<&str> = refused
        .iter()
        .map(|(_, name)| name.as_deref().unwrap_or("<unnamed>"))
        .collect();
    let remedies: Vec<String> = names
        .iter()
        .map(|name| format!("tcr login --codex --account {name}"))
        .collect();
    tracing::warn!(
        refused_accounts = names.join(", "),
        remedies = remedies.join("; "),
        "returning 403 to a Codex client — every account tried was refused with a 403; sign \
         each in again"
    );
    let message = codex_credential_refused_message(refused);
    codex_error_response(
        StatusCode::FORBIDDEN,
        codex_error_body(CodexRefusalKind::CredentialRefused, message).to_string(),
        None,
    )
}

fn codex_credential_refused_message(refused: &[(usize, Option<String>)]) -> String {
    refused.iter().map(|(_, name)| {
        let name = name.as_deref().unwrap_or("<unnamed>");
        format!("ChatGPT refused {name}'s login — sign it in again with `tcr login --codex --account {name}` or TcrBar's Re-login…")
    }).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests;

/// The routing document a launcher reads from the running proxy: which model
/// families each provider can serve now, behind the same local gate as the
/// status route.
#[cfg(test)]
mod forecast_routing_tests;
