//! The per-request state of the rotation ladder in [`super::handle_request`], and the one
//! answer it gives when it has no move left.

use std::collections::{HashMap, HashSet};

use axum::body::Body;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use serde_json::Value;
use tcr_status_wire::CodexRefusalKind;
use time::OffsetDateTime;

use crate::manager::Manager;
use crate::provider::Provider;

use super::client_error::{ClientError, ErrorKind};
use super::{
    ServedBy, bad_gateway, build_response, codex_credential_refused_message, codex_error_body,
    codex_error_response, codex_model_unavailable_response, codex_refused_pool_response,
    credential_refused_response, every_attempt_transport_failed, offline_unavailable,
};

/// What every attempt of one request came back with, counted across the whole ladder.
///
/// The terminal answer is read off these, never off the last attempt alone, so a request that
/// rotated through a refusal, a transport blip and a 429 is answered for all three.
#[derive(Default)]
pub(super) struct Tally {
    /// Attempts that died in transport, and attempts that got an upstream HTTP status back.
    ///
    /// These were ONE BOOL, and that was the bug: any single `send()` failure latched it for
    /// the rest of the request, and the check sits BEFORE both the soft-wait and the
    /// revalidation-serve, so one transport blip on one account disabled the entire recovery
    /// ladder and returned a 502 whose message ("every account transport-failed") was simply
    /// false. Observed live 09:16:46: the 502 fired while two accounts had already answered
    /// with honest 429s and a third served a 200 1.6s later. Counting BOTH outcomes keeps the
    /// 502 for the only case it actually describes: nothing ever reached an upstream at all.
    /// The latter is a 502, EXCEPT when `offline_failures` says the transport failures were
    /// this machine's resolver, which is a 503.
    pub(super) transport_failures: usize,
    pub(super) upstream_responses: usize,
    /// Set the moment any transport failure fails PAST the connect phase (`!err.is_connect()`,
    /// offline/DNS excluded: that is machine-level, not this). A connect-phase failure never
    /// left this box, so replaying it is free; a non-connect failure means a connection existed
    /// and the request may already have been written to it, so whether the upstream received
    /// or acted on it is unknown. One such attempt anywhere in the ladder taints the terminal
    /// answer for the whole request: see [`bad_gateway`]'s two shapes.
    pub(super) unknown_outcome_transport_failure: bool,
    /// How many name-resolution failures this request saw at all. Outlives the offline waits
    /// so the terminal answer can be the honest 503 even if the request later rotates for an
    /// unrelated reason.
    pub(super) offline_failures: usize,
    /// Accounts upstream refused with a 403 on OUR pooled credential, in the order seen. Every
    /// entry here is also an `upstream_responses` and a `tried`: a 403 is a real answer, not a
    /// transport gap, and there is nothing to retry on the same account (unlike 401, a fresh
    /// token does not change WHICH account this is). Kept as `(idx, name)` rather than folded
    /// into a bare counter because the fast-fail response must name the refused account(s),
    /// not just count them.
    pub(super) refused_403: Vec<(usize, Option<String>)>,
    /// Model access can differ between ChatGPT plans. A rejection benches the account for this
    /// request only: it may still serve another model on the very next turn.
    pub(super) model_refusals: Vec<(Option<String>, String)>,
    pub(super) account_access_blocks: usize,
    pub(super) codex_access_read_failures: usize,
    pub(super) last_account_access_block: Option<(HeaderMap, Bytes)>,
}

impl Tally {
    /// The answer the tallies alone decide, once the ladder has no move left: `Some` for a
    /// verdict that waiting or a fallback cannot change, `None` when the caller's own last
    /// resort (the soft wait and fallback, or the exhausted 429) is the honest answer.
    ///
    /// The one function both terminals call: select finding no account, and the attempt budget
    /// running out while still rotating. They were two copies, and the second had lost five of
    /// these arms.
    pub(super) fn verdict(
        &mut self,
        manager: &Manager,
        provider: Provider,
        model: Option<&str>,
        now: OffsetDateTime,
        bound_to: Option<usize>,
        strict_group: Option<&str>,
    ) -> Option<Response> {
        if every_attempt_transport_failed(self.transport_failures, self.upstream_responses) {
            // Nothing reached an upstream, but WHY decides the status. If any attempt died in
            // the resolver, the honest answer is the recoverable 503, not a 502 asserting the
            // upstream is unreachable when it is this machine that is off the network.
            // Reachable only if the offline arm's own bound was not what ended the walk (a
            // mixed request that rotated for another reason first).
            if self.offline_failures > 0 {
                return Some(offline_unavailable(provider, self.offline_failures));
            }
            return Some(bad_gateway(
                provider,
                self.transport_failures,
                self.unknown_outcome_transport_failure,
            ));
        }
        if !self.model_refusals.is_empty() {
            return Some(codex_model_unavailable_response(
                manager,
                model,
                &self.model_refusals,
                &self.refused_403,
            ));
        }
        if self.codex_access_read_failures > 0
            && self.codex_access_read_failures == self.upstream_responses
        {
            return Some(
                ClientError::new(
                    StatusCode::BAD_GATEWAY,
                    ErrorKind::Proxy,
                    "Could not read the upstream access-denied responses. The affected Codex accounts are held briefly; retry shortly.",
                )
                .respond(provider),
            );
        }
        if self.account_access_blocks > 0
            && self.account_access_blocks + self.refused_403.len() == self.upstream_responses
            && let Some((headers, body)) = self.last_account_access_block.take()
        {
            if !self.refused_403.is_empty() {
                let mut combined: Value = serde_json::from_slice(&body).unwrap_or_else(|_| {
                    codex_error_body(
                        CodexRefusalKind::CredentialRefused,
                        "ChatGPT refused workspace access.",
                    )
                });
                let message = combined
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("ChatGPT refused workspace access.");
                let message = format!(
                    "{message} {}",
                    codex_credential_refused_message(&self.refused_403)
                );
                if !combined.get("error").is_some_and(Value::is_object) {
                    combined["error"] = serde_json::json!({});
                }
                combined["error"]["message"] = message.into();
                return Some(codex_error_response(
                    StatusCode::FORBIDDEN,
                    combined.to_string(),
                    None,
                ));
            }
            return Some(build_response(
                StatusCode::FORBIDDEN,
                &headers,
                Body::from(body),
                ServedBy::PooledAccount,
            ));
        }
        // Every upstream answer this request got was a 403 refusing OUR credential: refusals
        // are the WHOLE story, not one dead account among others still worth a 429/quota wait.
        // Fast-fail here, before the soft-wait and revalidation-serve: those exist to ride out
        // a transient PARK, and a 403'd credential is not coming back on a timer. Gated on
        // `upstream_responses`, not on the account count, so this fires equally for a pinned
        // request that only ever tried its one pin. Mixed outcomes (some accounts refused,
        // others merely exhausted) fall through unchanged to the caller's exhaustion handling,
        // which can still recover on the accounts that were not refused.
        if !self.refused_403.is_empty() && self.refused_403.len() == self.upstream_responses {
            return Some(credential_refused_response(provider, &self.refused_403));
        }
        // A refused pool is an access problem, not a rate-limit retry. Keep that status
        // throughout the short revalidation hold, even near its end.
        match provider {
            Provider::Codex => {
                if manager.codex_pool_is_refused(now, bound_to, strict_group) {
                    manager
                        .note_codex_refusal(CodexRefusalKind::CredentialRefused, crate::now_ms());
                    return Some(codex_refused_pool_response());
                }
            }
            Provider::Anthropic => {}
        }
        None
    }
}

/// The per-request state of the rotation ladder: which accounts are benched, which one the
/// next attempt must use, every per-account and per-request retry budget, and the [`Tally`].
///
/// Its methods are the only way to change any of it, so a move that must touch two pieces of
/// state together (a timer park benches the account AND marks it re-admittable; a hold's
/// bench is remembered so a release re-admits exactly it) cannot be written with one half
/// missing.
#[derive(Default)]
pub(super) struct Ladder {
    /// The accounts select() may not return for this request.
    tried: HashSet<usize>,
    /// The accounts the bound-history hold benched, and only those, so a release
    /// mid-request can re-admit them without re-admitting what another rule benched: `tried`
    /// already carries the account-set scope of a borrowed request, and that bench is the
    /// lease's only enforcement on this path. Clearing `tried` wholesale served a lease scoped
    /// to one account on another (found in review, 2026-09-24).
    benched_for_hold: Vec<usize>,
    /// The account the NEXT iteration must use, bypassing select(). Three producers:
    ///
    ///  - A genuine SAME-account retry (401 force-refresh, transient-429 inline wait, 529
    ///    in-place backoff, a transport retry, an offline wait), which select() would
    ///    otherwise rotate AWAY from the very account the retry means to keep.
    ///  - The 529 FAILOVER, which parks the DIFFERENT account its availability probe already
    ///    chose. That probe IS this iteration's selection: re-running select() next iteration
    ///    would double its side effects (LRU stamp, divert log) and could race to a `None`
    ///    after we already benched the 529'd account in `tried`, turning a forwardable 529
    ///    into a synthesized 429.
    ///  - A released conversation's same-organization sibling (`same_org_release`, at the top
    ///    of the handler and at the mid-request release), selected for the same reason as the
    ///    failover.
    next_idx: Option<usize>,
    /// Accounts that already got their one force-refresh on a 401.
    forced_401: HashSet<usize>,
    /// Per-account inline-429 retry counters.
    retried_429: HashMap<usize, u32>,
    /// Per-account in-place 529 retry counters. Separate from `retried_429`: a 429 is "this
    /// account is over quota" and may rotate, a 529 is "the upstream is busy" and must not;
    /// sharing one counter would let one status spend the other's budget.
    retried_529: HashMap<usize, u32>,
    /// How many times THIS request has failed over to a DIFFERENT account on a 529.
    /// Per-REQUEST, deliberately not per-account like the counter above: what needs bounding
    /// is how much of the fleet one client request may walk, so it must not reset when the
    /// account changes.
    failovers_529: u32,
    /// Blind peer egress is offered ONCE per request, and this is the bound.
    ///
    /// It sits beside `Tally::unknown_outcome_transport_failure` because it is the same fact
    /// read the other way round: a carry is only ever offered while that flag is still false,
    /// nothing this request sent has left this box, so a resend down another path cannot
    /// double-send a POST. `src/peer/egress.rs` documents the seam; the offer itself is in the
    /// transport-failure arm, above both the resolver arm's sleep and the exhausted-transport
    /// terminal, so both entry points the design names are one call site rather than two
    /// that can drift.
    peer_egress_tried: bool,
    /// One-shot guard: at most one transient-fleet-park soft-wait per client request.
    soft_waited_exhaustion: bool,
    /// The subset of `tried` that is held out by a TRANSIENT 429 park and nothing else: the
    /// only entries the soft-wait is allowed to re-admit.
    ///
    /// `tried` alone cannot answer that question: it also carries accounts benched by a 401,
    /// a transport blip, a dead token, and a durable quota rejection. Only the transient park
    /// is a TIMER, so only it un-does itself when the clock runs out; re-admitting any of the
    /// others would re-send this request to an account that already failed it, in a loop. An
    /// account leaves this set the moment it is re-admitted, so membership always means
    /// "currently parked". A Codex 429 that is not a quota refusal parks the same way: its
    /// hold is a timer on the 429's own `retry-after`.
    parked_transient: HashSet<usize>,
    /// Same-account transport retries this request has spent, per account. The failing
    /// resource in a transport error is the CONNECTION, not the account: reqwest pools by
    /// `(scheme, authority)` and the account is only a Bearer header, so it is not in the
    /// pool key at all. Benching the account for the whole request therefore fails over on
    /// the wrong axis, and with a small eligible pool a single blip walks it to empty and
    /// answers a 502 (measured: 42 of 205 client-visible 502s fired at
    /// `transport_failures == 1`). A dead connection is evicted from the pool by the error
    /// itself, so the retry gets a fresh one.
    ///
    /// Bounded by `MAX_SAME_ACCOUNT_TRANSPORT_RETRIES`, not by one. This used to stop after a
    /// single retry because "the second failure IS evidence about the account"; a day's live
    /// log says otherwise: the first retry succeeds 94.7% of the time, so a second failure is
    /// overwhelmingly the SAME aging-connection fault recurring, not a sick account. Rotating
    /// instead costs a cold prompt cache, which is the expensive outcome this exists to avoid.
    transport_retried: HashMap<usize, u32>,
    /// Accounts whose client has already been recycled for a stream reset THIS request. An h2
    /// stream reset leaves the connection pooled (unlike a connection-level death), so the
    /// same-account retry would otherwise check out the identical broken connection every
    /// time; this recycles it once before the retry, not on every retry, so a burst of resets
    /// on one account rebuilds the client a single time rather than once per attempt.
    reset_recycled: HashSet<usize>,
    /// In-place waits this request has spent on a name-resolution failure. Per-REQUEST: an
    /// offline machine is one condition shared by every account, so there is nothing to
    /// count per account. `Tally::offline_failures` outlives the waits so the terminal answer
    /// can be the honest 503 even if the request later rotates for an unrelated reason.
    offline_waits: u32,
    tally: Tally,
}

impl Ladder {
    pub(super) fn tried(&self) -> &HashSet<usize> {
        &self.tried
    }

    pub(super) fn tally(&self) -> &Tally {
        &self.tally
    }

    /// Bench `idx` for the rest of this request; `false` if it already was.
    pub(super) fn bench(&mut self, idx: usize) -> bool {
        self.tried.insert(idx)
    }

    /// Bench `idx` for the bound-history hold, remembered so a release re-admits it.
    pub(super) fn bench_for_hold(&mut self, idx: usize) {
        if self.tried.insert(idx) {
            self.benched_for_hold.push(idx);
        }
    }

    /// Re-admit what the hold benched and only that: a lease's account-set scope stays
    /// benched, and so does the bound account, which failed this request.
    pub(super) fn release_hold(&mut self) {
        for idx in self.benched_for_hold.drain(..) {
            self.tried.remove(&idx);
        }
    }

    /// Park `idx` on a timer: benched, and re-admittable by the soft wait once its hold
    /// expires. The two always go together.
    pub(super) fn park(&mut self, idx: usize) {
        self.tried.insert(idx);
        self.parked_transient.insert(idx);
    }

    /// Re-admit every parked account whose hold has expired by `now_ms`. Returns how many
    /// were re-admitted and how many are still parked.
    pub(super) fn readmit_expired_parks(
        &mut self,
        manager: &Manager,
        now_ms: i64,
    ) -> (usize, usize) {
        let freed: Vec<usize> = self
            .parked_transient
            .iter()
            .copied()
            .filter(|&i| manager.hold_expired(i, now_ms))
            .collect();
        for i in &freed {
            self.tried.remove(i);
            self.parked_transient.remove(i);
        }
        (freed.len(), self.parked_transient.len())
    }

    /// The next attempt uses `idx` again.
    pub(super) fn retry_same(&mut self, idx: usize) {
        self.next_idx = Some(idx);
    }

    /// The next attempt uses `next`, already selected, or selects afresh when `None`.
    pub(super) fn set_next(&mut self, next: Option<usize>) {
        self.next_idx = next;
    }

    /// The account the next attempt must use, if a move named one.
    pub(super) fn take_next(&mut self) -> Option<usize> {
        self.next_idx.take()
    }

    /// `true` the first time `idx` asks for a 401 force-refresh in this request.
    pub(super) fn first_forced_refresh(&mut self, idx: usize) -> bool {
        self.forced_401.insert(idx)
    }

    /// Inline 429 waits already spent on `idx`.
    pub(super) fn retries_429(&self, idx: usize) -> u32 {
        self.retried_429.get(&idx).copied().unwrap_or(0)
    }

    pub(super) fn spend_429_retry(&mut self, idx: usize) {
        *self.retried_429.entry(idx).or_insert(0) += 1;
    }

    /// In-place 529 retries already spent on `idx`.
    pub(super) fn retries_529(&self, idx: usize) -> u32 {
        self.retried_529.get(&idx).copied().unwrap_or(0)
    }

    /// Spend one in-place 529 retry on `idx`; returns how many are now spent.
    pub(super) fn spend_529_retry(&mut self, idx: usize) -> u32 {
        let retried = self.retried_529.entry(idx).or_insert(0);
        *retried += 1;
        *retried
    }

    pub(super) fn failovers_529(&self) -> u32 {
        self.failovers_529
    }

    /// Fail this request over from the overloaded `idx` to `other`, which the availability
    /// probe already selected: `idx` benched, one failover spent, `other` next.
    pub(super) fn fail_over_529(&mut self, idx: usize, other: usize) {
        self.tried.insert(idx);
        self.failovers_529 += 1;
        self.next_idx = Some(other);
    }

    /// Same-account transport retries already spent on `idx`.
    pub(super) fn transport_retries(&self, idx: usize) -> u32 {
        self.transport_retried.get(&idx).copied().unwrap_or(0)
    }

    /// Spend one same-account transport retry on `idx`; returns how many are now spent.
    pub(super) fn spend_transport_retry(&mut self, idx: usize) -> u32 {
        let spent = self.transport_retried.entry(idx).or_insert(0);
        *spent += 1;
        *spent
    }

    /// `true` the first time `idx`'s client is recycled for a stream reset in this request.
    pub(super) fn first_reset_recycle(&mut self, idx: usize) -> bool {
        self.reset_recycled.insert(idx)
    }

    /// Spend one offline wait if the request has one left; returns its ordinal.
    pub(super) fn take_offline_wait(&mut self, max: u32) -> Option<u32> {
        (self.offline_waits < max).then(|| {
            self.offline_waits += 1;
            self.offline_waits
        })
    }

    pub(super) fn peer_egress_tried(&self) -> bool {
        self.peer_egress_tried
    }

    pub(super) fn spend_peer_egress(&mut self) {
        self.peer_egress_tried = true;
    }

    pub(super) fn soft_waited_exhaustion(&self) -> bool {
        self.soft_waited_exhaustion
    }

    pub(super) fn spend_soft_wait(&mut self) {
        self.soft_waited_exhaustion = true;
    }

    pub(super) fn transport_failed(&mut self) {
        self.tally.transport_failures += 1;
    }

    pub(super) fn upstream_answered(&mut self) {
        self.tally.upstream_responses += 1;
    }

    /// A transport failure past the connect phase: its outcome upstream is unknown.
    pub(super) fn unknown_outcome(&mut self) {
        self.tally.unknown_outcome_transport_failure = true;
    }

    /// A name-resolution failure; returns how many this request has seen.
    pub(super) fn offline_failed(&mut self) -> usize {
        self.tally.offline_failures += 1;
        self.tally.offline_failures
    }

    pub(super) fn codex_access_unread(&mut self) {
        self.tally.codex_access_read_failures += 1;
    }

    /// A Codex 403 for the account's access, with the answer kept to forward when every
    /// answer was one.
    pub(super) fn account_access_blocked(&mut self, answer: Option<(HeaderMap, Bytes)>) {
        self.tally.account_access_blocks += 1;
        self.tally.last_account_access_block = answer;
    }

    pub(super) fn refused_403(&mut self, idx: usize, name: Option<String>) {
        self.tally.refused_403.push((idx, name));
    }

    pub(super) fn model_refused(&mut self, account: Option<String>, detail: String) {
        self.tally.model_refusals.push((account, detail));
    }

    /// [`Tally::verdict`], for the two terminals.
    pub(super) fn verdict(
        &mut self,
        manager: &Manager,
        provider: Provider,
        model: Option<&str>,
        now: OffsetDateTime,
        bound_to: Option<usize>,
        strict_group: Option<&str>,
    ) -> Option<Response> {
        self.tally
            .verdict(manager, provider, model, now, bound_to, strict_group)
    }
}
