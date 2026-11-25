//! Verbs that print one block the running proxy reports and have no offline fallback:
//! `tcr sessions`, `tcr peer status` and `tcr routing --json`.

use std::path::Path;

use super::live_client::{
    LiveStatusError, Unreached, fetch_live_status, fetch_live_status_body, parse_live_status,
    send_loopback_with_redirect_policy,
};
use super::load_config;
use super::status::skew_report;
use crate::config::Config;

/// The peers block as the RUNNING proxy reports it, `tcr peer status --json`.
///
/// # Why this is a separate verb from `tcr peer ls`
///
/// `peer ls` projects two FILES and answers the same thing with no server at
/// all. This asks the process that is serving what it currently holds: the
/// leases in its ledger, when each Mac was last heard from, the paths it knows.
/// Folding them into one verb would make a proxy that is DOWN look like a peers
/// file that is EMPTY, which is the one confusion the Peers tab forbids by name
/// (`TcrBarCore/PeerListDocument.swift`, `LivePeersRead`).
///
/// So the error is returned rather than turned into an empty list: the caller
/// exits non-zero, and the panel keeps the file half it already drew.
pub async fn live_peers(
    config: &Config,
) -> Result<Vec<crate::peer::status::rows::PeerStatusRow>, String> {
    match fetch_live_status(config).await {
        Ok(payload) => Ok(payload.peers),
        Err(LiveStatusError::NoServer) => Err(format!(
            "no tcr is listening on 127.0.0.1:{}, so there is no live view to read \
             (`tcr peer ls` reads the file)",
            config.proxy.port
        )),
        Err(LiveStatusError::NoAnswer(why)) => Err(format!(
            "the tcr on 127.0.0.1:{} accepted the connection and never answered: {why}",
            config.proxy.port
        )),
        Err(LiveStatusError::Unusable(why)) => Err(format!(
            "the tcr on 127.0.0.1:{} answered with something this build cannot read: {why}",
            config.proxy.port
        )),
    }
}

/// Why `tcr routing --json` printed no document.
#[derive(Debug)]
pub(crate) enum RoutingReadError {
    /// The connection was refused: no proxy is listening.
    NoServer,
    /// Something may be there, but nothing it gave back is a readable schema-1 document: it
    /// refused the key, answered an HTTP error, did not answer in time, or sent something else.
    Unreadable(String),
}

impl RoutingReadError {
    /// `tcr routing --json`'s exit: 3 when no proxy is listening, 4 for every other way of not
    /// reading the document; 0 is the document printed. A launcher branches on these, so they
    /// are a contract. 1 stays every verb's own error, an unreadable config among them.
    pub(crate) const fn exit_code(&self) -> i32 {
        match self {
            Self::NoServer => 3,
            Self::Unreadable(_) => 4,
        }
    }

    fn why(&self) -> &str {
        match self {
            Self::NoServer => "nothing is listening",
            Self::Unreadable(why) => why,
        }
    }
}

impl From<Unreached> for RoutingReadError {
    fn from(unreached: Unreached) -> Self {
        match unreached {
            Unreached::Refused => Self::NoServer,
            Unreached::Silent(why) | Unreached::Unbuilt(why) => Self::Unreadable(why),
        }
    }
}

/// The running proxy's [`crate::proxy::ROUTING_PATH`] answer, as the bytes it sent, once
/// [`crate::routing::parse`] has read them as schema 1.
pub(super) async fn fetch_routing_body(config: &Config) -> Result<String, RoutingReadError> {
    let response = send_loopback_with_redirect_policy(
        config,
        crate::proxy::ROUTING_PATH,
        reqwest::redirect::Policy::none(),
        |client, url| client.get(url),
    )
    .await?;
    let status = response.status();
    if !status.is_success() {
        let hint = if status.as_u16() == 401 {
            " — the proxy api-key in the config was rejected"
        } else {
            ""
        };
        return Err(RoutingReadError::Unreadable(format!("HTTP {status}{hint}")));
    }
    let body = response
        .text()
        .await
        .map_err(|e| RoutingReadError::Unreadable(format!("reading the response body: {e}")))?;
    crate::routing::parse(&body).map_err(RoutingReadError::Unreadable)?;
    Ok(body)
}

/// `tcr routing --json`: print the running proxy's routing document ([`crate::routing`]) and
/// return the exit code ([`RoutingReadError::exit_code`]).
///
/// On failure stdout stays empty and the reason goes to stderr. Nothing is built offline:
/// whether a model is routable is the running selector's answer, and a document made without
/// that selector would describe one that is not serving.
pub async fn routing(config_path: &Path) -> anyhow::Result<i32> {
    // Read-only verb: plain load, no clobber-warning (we never save).
    let config = load_config(config_path)?;
    match fetch_routing_body(&config).await {
        Ok(body) => {
            println!("{}", body.trim_end());
            Ok(0)
        }
        Err(error) => {
            eprintln!(
                "[tcr] could not read the routing document from the proxy on 127.0.0.1:{} ({})",
                config.proxy.port,
                error.why()
            );
            Ok(error.exit_code())
        }
    }
}

/// `tcr status [--json]` — render the fleet as greppable text or a JSON array,
/// preferring the RUNNING proxy's own numbers.
///
/// The live endpoint is tried first because it is the only place the serving
/// counters exist: requests, tokens and the prompt-cache hit ratio are per-process
/// state, and an offline snapshot's copies are structurally zero. When no server
/// answers we fall back to the historical offline path — a fresh `Manager` plus a
/// live quota probe, never persisted — and every rendering says which of the two
/// it is, so a zero counter can never again pass for a measurement.
///
/// The live path deliberately does NOT probe. Quota there comes from the server's
/// own probe loop (per account, on a random schedule centred on
/// `quotaProbeSeconds`, 300s by default), which is both
/// fresher in practice than a cold one-shot probe and one fewer caller hitting the
/// usage endpoint — that endpoint rate-limits, and a second prober racing the
/// server's is what makes a whole fleet read `probe=rate-limited`. Only the
/// offline fallback, which has no server to inherit quota from, probes.
/// Did this status body CARRY a top-level `sessions` key?
///
/// The one question the typed [`StatusPayload`] cannot answer. Its `sessions`
/// field is `#[serde(default)]`, which is what lets a new client keep reading
/// an old server at all, and the price of that is that "the server never
/// reported sessions" and "the server reported none" arrive as the same empty
/// vec. The panel draws two different sentences for those two facts, so the
/// distinction has to survive the trip — read here off the raw body.
///
/// A body that is not JSON reads `false`. It never reaches this function in
/// practice ([`parse_live_status`] rejects it first), and `false` is the
/// conservative answer either way: it claims no channel rather than a broken one.
pub(super) fn payload_carries_sessions(body: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("sessions").cloned())
        .is_some()
}

/// The `tcr sessions --json` document: `{"supported": bool, "sessions": [...]}`.
///
/// The rows cross verbatim as the wire emitted them — [`tcr_status_wire::SessionRow`]
/// is the shared type both the proxy's `/_tcr/status` response and the panel's
/// Swift decoder are written against, so re-shaping them here would be a second
/// contract to keep in step with the first.
///
/// An OBJECT, deliberately, where `tcr status --json` is a bare array: the
/// `supported` flag has nowhere to live in an array, and putting it there is
/// what would have forced a breaking change on `status`'s existing contract.
///
/// `heat` rides here too, as a `heat` key present only when the server read one
/// ([`crate::status::StatusPayload::heat`]). The panel cannot read the proxy's
/// endpoint itself (it never holds the proxy key), so it needs a shell-out, and
/// this object is the one it already makes on every poll that has a top level
/// for a machine-wide fact; `status --json` is a bare array of accounts.
///
/// `fanControl` rides beside `heat` the same way and for the same reason: the popover reads
/// what the server's fan controller is doing from this poll.
pub(super) fn render_sessions_json(
    supported: bool,
    sessions: &[tcr_status_wire::SessionRow],
    heat: Option<&tcr_status_wire::HeatRow>,
    fan_control: Option<&tcr_status_wire::FanControlRow>,
) -> String {
    let mut document = serde_json::json!({ "supported": supported, "sessions": sessions });
    if let Some(heat) = heat {
        document["heat"] = serde_json::json!(heat);
    }
    if let Some(fan_control) = fan_control {
        document["fanControl"] = serde_json::json!(fan_control);
    }
    document.to_string()
}

/// The `tcr sessions` text form: a `supported=… sessions=…` header, then one line per
/// session, every line ending in a newline. A pure function, so a test can pin the bytes
/// a person reads without a running proxy. A Codex row adds ` provider=codex` after its
/// account and ` reasoningTokens=` when non-zero, and prints its cost through
/// [`session_cost`], while a Claude row prints exactly as before.
pub(super) fn render_sessions_text(
    supported: bool,
    sessions: &[tcr_status_wire::SessionRow],
) -> String {
    let mut out = format!("supported={supported} sessions={}\n", sessions.len());
    for row in sessions {
        out.push_str(&format!(
            "session={} account={}{} model={} requests={} inputTokens={} outputTokens={} cacheReadTokens={} toolCalls={} costUsd={}{}\n",
            row.session_id,
            row.account.as_deref().unwrap_or("-"),
            row.provider
                .as_deref()
                .map_or_default(|p| format!(" provider={p}")),
            row.model.as_deref().unwrap_or("-"),
            row.requests,
            row.input_tokens,
            row.output_tokens,
            row.cache_read_tokens,
            row.tools.calls,
            session_cost(row),
            if row.reasoning_tokens > 0 {
                format!(" reasoningTokens={}", row.reasoning_tokens)
            } else {
                String::new()
            },
        ));
    }
    out
}

/// The value of `costUsd=` on a `tcr sessions` line. A Claude row prints its list-price
/// figure as it always has, `0.0000` for a model this build cannot price included. A Codex
/// row never prints a zero nobody measured. It reads `n/a` until one of its requests is
/// priced, then the priced part, with the count it leaves out while some still are not.
fn session_cost(row: &tcr_status_wire::SessionRow) -> String {
    if row.provider.is_none() || (row.cost_usd > 0.0 && row.unpriced_requests == 0) {
        format!("{:.4}", row.cost_usd)
    } else if row.cost_usd > 0.0 {
        format!(
            "{:.4} unpricedRequests={}",
            row.cost_usd, row.unpriced_requests
        )
    } else {
        "n/a".to_string()
    }
}

/// `tcr sessions [--json]` — the sessions the RUNNING proxy has seen in the
/// last hour.
///
/// Live-only, with no offline fallback, and that is the whole design: a session
/// is the running proxy's own state, and the copy it keeps on disk
/// ([`crate::session_wire_persist`]) is a cache read back at boot, not a record
/// of what is live, so an offline rendering could only ever be a stale list
/// dressed as a measurement. An unreachable server is an error exit instead —
/// the same failure `status` warns about before falling back, except here there
/// is nothing to fall back to.
///
/// `supported` answers "did this server's payload CARRY a `sessions` key", not
/// "is the array non-empty". [`StatusPayload::sessions`] is `#[serde(default)]`
/// (so that a client built after F1 keeps reading a server built before it),
/// which means a pre-F1 server and a completely idle server both decode to the
/// same empty vec. Only the raw body can separate them, so the key presence is
/// read off a [`serde_json::Value`] of the SAME response the typed parse used —
/// never a second request, which could see a different server.
pub async fn sessions(config_path: &Path, json: bool) -> anyhow::Result<()> {
    // Read-only verb: plain load, no clobber-warning (we never save).
    let config = load_config(config_path)?;

    let body = match fetch_live_status_body(&config).await {
        Ok(body) => body,
        Err(reason) => {
            let why = reason.why();
            anyhow::bail!(
                "could not read live status from the proxy on :{} ({why}) — sessions live only in the running process, so there is no offline snapshot to fall back to.",
                config.proxy.port
            );
        }
    };
    let payload = match parse_live_status(&body) {
        Ok(payload) => payload,
        Err(reason) => {
            let why = reason.why();
            anyhow::bail!(
                "could not read live status from the proxy on :{} ({why}) — sessions live only in the running process, so there is no offline snapshot to fall back to.",
                config.proxy.port
            );
        }
    };

    // Key presence, not array length — see this function's doc-comment.
    let supported = payload_carries_sessions(&body);

    // Build skew goes to STDERR in both modes, exactly as `status` does: the
    // `--json` stdout is a single object a caller pipes into jq, and a
    // diagnostic printed there would corrupt it.
    if let Some(line) = skew_report(Some(&payload.build)) {
        eprintln!("{line}");
    }

    // A heat object this build cannot read is left out of the document and
    // said on stderr, beside the skew line, rather than failing the command.
    let heat = payload.heat_reading();
    if let Err(crate::status::Unread::Unparsed(why)) = heat {
        eprintln!("tcr: the server's heat reading did not parse, so it is left out: {why}");
    }
    let fan_control = payload.fan_control_reading();
    if let Err(crate::status::Unread::Unparsed(why)) = fan_control {
        eprintln!("tcr: the server's fanControl object did not parse, so it is left out: {why}");
    }

    if json {
        // The rows cross verbatim as the wire emitted them — `SessionRow` is
        // the shared `tcr-status-wire` type both the proxy and the panel's
        // Swift decoder are written against, so re-shaping it here would be a
        // second contract to keep in step with the first.
        println!(
            "{}",
            render_sessions_json(supported, &payload.sessions, heat.ok(), fan_control.ok())
        );
    } else {
        print!("{}", render_sessions_text(supported, &payload.sessions));
    }
    Ok(())
}
