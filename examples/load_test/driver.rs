//! The fake Claude Code sessions: each has a stable `metadata.user_id`, the way Claude Code
//! sends it, and echoes its last reply's signed thinking block on the next turn, so the
//! proxy's bound-token lookup runs on every request.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::Semaphore;

use crate::plan::{Kind, Planned};
use crate::stub::{Stub, TAG_HEADER};

/// The placeholder key `tcr run` exports: on loopback it is not an identity, so the body's
/// `user_id` keys the session (`stable_session_key`, `src/proxy.rs`).
const CLIENT_KEY: &str = "via-tcr";

/// The `anthropic-beta` flags Claude Code sends on an OAuth session. In this order because
/// gitleaks' generic-api-key rule reads `oauth-...,<value>` as a credential and its value.
const BETAS: [&str; 2] = ["interleaved-thinking-2025-05-14", "oauth-2025-04-20"];

/// The most requests in flight at once. Past this the runaway phase queues in the driver
/// rather than opening thousands of sockets.
const MAX_IN_FLIGHT: usize = 512;

/// One request's outcome, as the client saw it.
#[derive(Debug, Clone)]
pub struct Record {
    /// What went wrong below HTTP: a refused connection, a reset, a cut stream.
    pub error: Option<String>,
    pub status: Option<u16>,
    pub ok: bool,
    pub ttfb_ms: Option<f64>,
    pub total_ms: f64,
    /// Total minus the stub's own time: what the proxy added.
    pub added_ms: Option<f64>,
}

pub struct Session {
    user_id: String,
    last: Mutex<Option<(String, String)>>,
}

pub struct Driver {
    client: reqwest::Client,
    base: String,
    stub: Arc<Stub>,
    sessions: Vec<Session>,
    padding: String,
    next_tag: AtomicU64,
    gate: Arc<Semaphore>,
}

impl Driver {
    pub fn new(
        proxy_port: u16,
        stub: Arc<Stub>,
        sessions: usize,
        body_kb: usize,
    ) -> anyhow::Result<Arc<Self>> {
        let client = reqwest::Client::builder()
            // The driver talks to the proxy on loopback and nowhere else; an inherited
            // HTTPS_PROXY must not reroute it.
            .no_proxy()
            .pool_max_idle_per_host(MAX_IN_FLIGHT)
            .timeout(Duration::from_secs(300))
            .build()?;
        let sessions = (0..sessions)
            .map(|i| Session {
                user_id: json!({
                    "device_id": format!("{:064x}", 0xdead_beef_u64 + i as u64),
                    "account_uuid": "11111111-1111-1111-1111-111111111111",
                    "session_id": format!("33333333-3333-3333-3333-{:012}", i),
                })
                .to_string(),
                last: Mutex::new(None),
            })
            .collect();
        Ok(Arc::new(Self {
            client,
            base: format!("http://127.0.0.1:{proxy_port}"),
            stub,
            sessions,
            padding: "lorem ipsum ".repeat(body_kb * 1024 / 12),
            next_tag: AtomicU64::new(1),
            gate: Arc::new(Semaphore::new(MAX_IN_FLIGHT)),
        }))
    }

    fn body(&self, session: &Session, kind: Kind) -> Value {
        let last = session
            .last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut messages = vec![json!({"role": "user", "content": "Start the task."})];
        if let Some((_id, signature)) = last {
            messages.push(json!({"role": "assistant", "content": [
                {"type": "thinking", "thinking": "Considering the request.", "signature": signature},
                {"type": "text", "text": "Done."}
            ]}));
            messages.push(json!({"role": "user", "content": "Continue."}));
        }
        let mut body = json!({
            "model": "claude-opus-5-5",
            "max_tokens": 32_000,
            "metadata": {"user_id": session.user_id},
            "system": [{"type": "text", "text": self.padding}],
            "messages": messages,
        });
        if kind == Kind::Streamed {
            body["stream"] = json!(true);
        }
        if kind == Kind::CountTokens {
            body.as_object_mut().map(|o| o.remove("max_tokens"));
            body.as_object_mut().map(|o| o.remove("metadata"));
        }
        body
    }

    /// Send one request and read its whole answer.
    pub async fn send(self: &Arc<Self>, session_index: usize, kind: Kind) -> Record {
        let session = &self.sessions[session_index % self.sessions.len()];
        let tag = self.next_tag.fetch_add(1, Ordering::Relaxed);
        let path = match kind {
            Kind::CountTokens => "/v1/messages/count_tokens?beta=true",
            Kind::Streamed | Kind::Buffered => "/v1/messages?beta=true",
        };
        let body = self.body(session, kind);
        let started = Instant::now();
        let sent = self
            .client
            .post(format!("{}{path}", self.base))
            .header("x-api-key", CLIENT_KEY)
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", BETAS.join(","))
            .header("user-agent", "claude-cli/2.1.300 (external, cli)")
            .header("x-app", "cli")
            .header(TAG_HEADER, tag.to_string())
            .json(&body)
            .send()
            .await;
        let mut response = match sent {
            Ok(response) => response,
            Err(err) => {
                return Record {
                    error: Some(format!("send: {err}")),
                    status: None,
                    ok: false,
                    ttfb_ms: None,
                    total_ms: ms(started.elapsed()),
                    added_ms: None,
                }
            }
        };
        let status = response.status().as_u16();
        let mut ttfb = None;
        let mut bytes = Vec::new();
        let mut read_error = None;
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    ttfb.get_or_insert_with(|| ms(started.elapsed()));
                    bytes.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(err) => {
                    read_error = Some(format!("body: {err}"));
                    break;
                }
            }
        }
        let total = started.elapsed();
        let ok = read_error.is_none() && (200..300).contains(&status);
        if ok && kind != Kind::CountTokens {
            if let Some(minted) = minted_tokens(&bytes) {
                *session
                    .last
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(minted);
            }
        }
        let added = self
            .stub
            .take_span(tag)
            .map(|span| ms(total.saturating_sub(span.end.saturating_duration_since(span.start))));
        Record {
            error: read_error,
            status: Some(status),
            ok,
            ttfb_ms: ttfb,
            total_ms: ms(total),
            added_ms: if ok { added } else { None },
        }
    }

    /// Open loop: send each planned request at its time, whatever is still in flight, and
    /// wait up to `drain` after the last send for the stragglers.
    pub async fn run_schedule(
        self: &Arc<Self>,
        plan: &[Planned],
        drain: Duration,
    ) -> (Vec<Record>, usize) {
        let origin = Instant::now();
        let mut tasks = tokio::task::JoinSet::new();
        for planned in plan {
            let due = origin + Duration::from_secs_f64(planned.at_ms / 1_000.0);
            tokio::time::sleep_until(due.into()).await;
            let Ok(permit) = self.gate.clone().acquire_owned().await else {
                break;
            };
            let me = self.clone();
            let p = *planned;
            tasks.spawn(async move {
                let record = me.send(p.session, p.kind).await;
                drop(permit);
                record
            });
        }
        collect(tasks, drain).await
    }

    /// Closed loop: `workers` sessions each send their next request when the last one ends,
    /// until `duration` is up.
    pub async fn run_closed(
        self: &Arc<Self>,
        workers: usize,
        duration: Duration,
        seed: u64,
    ) -> (Vec<Record>, usize) {
        let deadline = Instant::now() + duration;
        let mut tasks = tokio::task::JoinSet::new();
        for w in 0..workers {
            let me = self.clone();
            tasks.spawn(async move {
                let mut rng = crate::stats::Rng::new(seed.wrapping_add(w as u64 * 7919));
                let mut out = Vec::new();
                while Instant::now() < deadline {
                    out.push(me.send(w, Kind::draw(&mut rng)).await);
                }
                out
            });
        }
        let mut records = Vec::new();
        while let Some(joined) = tasks.join_next().await {
            if let Ok(batch) = joined {
                records.extend(batch);
            }
        }
        (records, 0)
    }

    /// A few requests per session before measuring, so connection pools and pins are warm.
    pub async fn warm_up(self: &Arc<Self>, sessions: usize) {
        let mut tasks = tokio::task::JoinSet::new();
        for s in 0..sessions {
            let me = self.clone();
            tasks.spawn(async move { me.send(s, Kind::Streamed).await });
        }
        while tasks.join_next().await.is_some() {}
    }
}

/// Join every task, giving up on the ones still running after `drain`. Returns the records
/// and how many requests never finished.
async fn collect(mut tasks: tokio::task::JoinSet<Record>, drain: Duration) -> (Vec<Record>, usize) {
    let mut records = Vec::new();
    let deadline = tokio::time::Instant::now() + drain;
    loop {
        match tokio::time::timeout_at(deadline, tasks.join_next()).await {
            Ok(Some(Ok(record))) => records.push(record),
            Ok(Some(Err(_))) => {}
            Ok(None) => break,
            Err(_) => {
                let unfinished = tasks.len();
                tasks.abort_all();
                return (records, unfinished);
            }
        }
    }
    (records, 0)
}

/// The message id and signature a reply minted, read from either reply shape.
fn minted_tokens(body: &[u8]) -> Option<(String, String)> {
    if let Ok(value) = serde_json::from_slice::<Value>(body) {
        let id = value["id"].as_str()?.to_string();
        let signature = value["content"][0]["signature"].as_str()?.to_string();
        return Some((id, signature));
    }
    let text = std::str::from_utf8(body).ok()?;
    let mut id = None;
    let mut signature = None;
    for data in text.lines().filter_map(|l| l.strip_prefix("data: ")) {
        let Ok(event) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        if let Some(found) = event["message"]["id"].as_str() {
            id = Some(found.to_string());
        }
        if let Some(found) = event["delta"]["signature"].as_str() {
            signature = Some(found.to_string());
        }
    }
    Some((id?, signature?))
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minted_tokens_are_read_from_both_reply_shapes() {
        let sse: Vec<u8> = crate::stub::sse_chunks(4).concat();
        assert_eq!(
            minted_tokens(&sse),
            Some((crate::stub::message_id(4), crate::stub::signature(4)))
        );
        let json = crate::stub::message_json(5).to_string();
        assert_eq!(
            minted_tokens(json.as_bytes()),
            Some((crate::stub::message_id(5), crate::stub::signature(5)))
        );
        assert_eq!(minted_tokens(b"not a reply"), None);
    }
}
