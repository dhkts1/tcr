//! The per-request state of the rotation ladder in [`super::handle_request`], and the one
//! answer it gives when it has no move left.

use axum::body::Body;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use serde_json::Value;
use tcr_status_wire::CodexRefusalKind;
use time::OffsetDateTime;

use crate::manager::Manager;
use crate::provider::Provider;

use super::{
    ServedBy, bad_gateway, build_response, codex_credential_refused_message, codex_error_body,
    codex_error_response, codex_model_unavailable_response, codex_refused_pool_response,
    credential_refused_response, error_response, every_attempt_transport_failed,
    offline_unavailable,
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
                return Some(offline_unavailable(self.offline_failures));
            }
            return Some(bad_gateway(
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
            return Some(error_response(
                StatusCode::BAD_GATEWAY,
                "proxy_error",
                "Could not read the upstream access-denied responses. The affected Codex accounts are held briefly; retry shortly.",
                None,
            ));
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
