//! The bookkeeping of one served request, written once for every path that serves one: the
//! direct send's terminal outcome, its two usage tails (streamed and buffered), and a request
//! a peer carried ([`crate::peer::egress::record_carried`]).

use axum::http::StatusCode;
use time::OffsetDateTime;

use crate::manager::Manager;
use crate::provider::Provider;
use crate::session_wire::{ToolResultEvent, ToolUseEvent};
use crate::stats::{RequestLogEntry, SessionKind};

use super::{ParsedUsage, TimingFields, usage_record};

/// What a served request folds into the wire-session table, which differs by provider.
pub(crate) enum WireFold<'a> {
    /// The request's model and the tool events parsed out of its body.
    Anthropic {
        model: Option<String>,
        tool_uses: &'a [ToolUseEvent],
        tool_results: &'a [ToolResultEvent],
    },
    /// The query-stripped path: Codex request bodies carry no tool events here, and only a
    /// Responses turn closes calls started by the previous response; auxiliary model calls
    /// leave them running.
    Codex { path: &'a str },
}

impl WireFold<'_> {
    const fn provider(&self) -> Provider {
        match self {
            Self::Anthropic { .. } => Provider::Anthropic,
            Self::Codex { .. } => Provider::Codex,
        }
    }
}

/// One request an account served, as the served counter, the wire-session table and the ring
/// buffer `tcr status` and the TUI read all record it.
pub(crate) struct ServedRequest<'a> {
    pub(crate) idx: usize,
    pub(crate) served_at: OffsetDateTime,
    pub(crate) session_key: Option<u64>,
    pub(crate) session_kind: SessionKind,
    pub(crate) wire_session_id: Option<&'a str>,
    pub(crate) account: Option<String>,
    /// The method and the path with its query, as the ring buffer shows them.
    pub(crate) method: String,
    pub(crate) path_and_query: String,
    pub(crate) status: u16,
    /// The provider the request was routed on is the one this names.
    pub(crate) wire: WireFold<'a>,
}

/// Count the request once against the account that served it, fold it into the wire-session
/// table (a no-op when it carries no session id), and log it to the ring buffer.
pub(crate) fn record_served_request(manager: &Manager, served: ServedRequest<'_>) {
    manager.record_served(
        served.idx,
        served.served_at,
        served.session_key,
        served.session_kind,
    );
    let provider = served.wire.provider();
    match served.wire {
        WireFold::Codex { path } => manager.record_codex_wire_request(
            served.wire_session_id,
            served.account.clone(),
            path,
            served.served_at,
        ),
        WireFold::Anthropic {
            model,
            tool_uses,
            tool_results,
        } => manager.record_wire_session(
            served.wire_session_id,
            served.account.clone(),
            model,
            served.served_at,
            tool_uses,
            tool_results,
        ),
    }
    manager.push_log(RequestLogEntry {
        time: served.served_at,
        method: served.method,
        path: served.path_and_query,
        status: served.status,
        account: served.account.unwrap_or_default(),
        provider,
    });
}

/// A 2xx that reported usage, as both usage tails need it. Owned, because the streamed tail
/// runs in a task spawned off the response that outlives the attempt that served it.
pub(super) struct ServedTurn {
    pub(super) provider: Provider,
    pub(super) idx: usize,
    pub(super) account_name: Option<String>,
    pub(super) path: String,
    pub(super) session_key: Option<u64>,
    pub(super) wire_session_id: Option<String>,
    /// The model the request's body named, which wins over the response's own.
    pub(super) request_model: Option<String>,
    /// Whether the request's one parse found a 1-hour cache breakpoint.
    pub(super) extended_ttl: bool,
}

impl ServedTurn {
    /// The `request usage` line and the usage ledger for a turn that reported usage. One
    /// line per terminal request, never per SSE event (a per-event line would be a flood on
    /// a long erroring stream), carrying the request's timing rather than a second INFO line.
    /// `account_index`, `path` and `status` are the fields the DEBUG `serving request` and
    /// `upstream response` lines carry, so this one line is a served request's whole INFO
    /// record; `scripts/` parse it.
    ///
    /// The model is the REQUEST's whenever its body named one. Only Codex falls back to
    /// `usage_model`: its body is usually zstd and does not parse. This model belongs to
    /// the retained usage; catalogue observations carry response identity separately.
    pub(super) fn finish(
        &self,
        manager: &Manager,
        status: StatusCode,
        parsed: &ParsedUsage,
        usage_model: Option<String>,
        timing: Option<&TimingFields>,
    ) {
        let record = usage_record(
            parsed,
            self.request_model.clone().or_else(|| {
                (self.provider == Provider::Codex)
                    .then_some(usage_model)
                    .flatten()
            }),
            self.session_key,
            || self.extended_ttl,
        );
        tracing::info!(
            account = self.account_name.as_deref().unwrap_or("?"),
            account_index = self.idx,
            path = %self.path,
            status = status.as_u16(),
            session = ?self.session_key,
            model = record.model.as_deref().unwrap_or("?"),
            cache_read_tokens = parsed.cache_read,
            cache_creation_tokens = parsed.cache_creation,
            cache_creation_1h_tokens = record.cache_1h,
            input_tokens = parsed.input_total,
            output_tokens = parsed.output,
            wait_ms = timing.and_then(|t| t.wait_ms),
            throttle_ms = timing.map(|t| t.throttle_ms),
            upstream_ms = timing.and_then(|t| t.upstream_ms),
            first_byte_ms = timing.and_then(|t| t.first_byte_ms),
            total_ms = timing.map(|t| t.total_ms),
            attempts = timing.map(|t| t.attempts),
            switched = timing.map(|t| t.switched),
            completed = timing.map(|t| t.completed),
            "request usage"
        );
        if let Some(t) = timing {
            t.log_if_held();
        }
        // A Codex session also keeps the turn's reasoning tokens, and its model is the one the
        // ledger files the turn under.
        let session_id = self.wire_session_id.as_deref();
        match self.provider {
            Provider::Codex => manager.record_codex_wire_session_usage(session_id, &record),
            Provider::Anthropic => manager.record_wire_session_usage(session_id, &record),
        }
        manager.record_usage(self.idx, record);
    }
}
