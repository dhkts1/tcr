//! A fake Anthropic on `127.0.0.1:0`: `/v1/messages` streamed and buffered,
//! `count_tokens`, and the OAuth profile, with the usage and rate-limit headers a live
//! reply carries. It records when each request arrived and when its answer ended, so the
//! driver can subtract the stub's own time from what the client saw.

use std::collections::{BTreeMap, HashMap};
use std::convert::Infallible;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::response::Response;
use axum::Router;
use serde_json::{json, Value};

/// The header the driver tags each request with, read back here to pair the two clocks.
pub const TAG_HEADER: &str = "x-load-test-id";

/// Input and output token counts every reply reports, so the proxy's usage parsing has
/// real numbers to read.
pub const INPUT_TOKENS: u64 = 1_200;
pub const CACHE_READ_TOKENS: u64 = 48_000;
pub const OUTPUT_TOKENS: u64 = 350;

/// When the stub began and finished one request.
#[derive(Debug, Clone, Copy)]
pub struct Span {
    pub start: Instant,
    pub end: Instant,
}

pub struct Stub {
    /// Time from a `/v1/messages` arrival to its last byte. 40% of it passes before the
    /// first byte, the rest between the stream's chunks.
    pub reply_time: Duration,
    spans: Mutex<HashMap<u64, Span>>,
    arrivals: Mutex<Vec<Instant>>,
    unexpected: Mutex<BTreeMap<String, usize>>,
    untagged: AtomicU64,
    counter: AtomicU64,
}

impl Stub {
    pub fn new(reply_time: Duration) -> Arc<Self> {
        Arc::new(Self {
            reply_time,
            spans: Mutex::new(HashMap::new()),
            arrivals: Mutex::new(Vec::new()),
            unexpected: Mutex::new(BTreeMap::new()),
            untagged: AtomicU64::new(0),
            counter: AtomicU64::new(0),
        })
    }

    /// The span recorded for the request tagged `id`, removed so the map stays small.
    pub fn take_span(&self, id: u64) -> Option<Span> {
        lock(&self.spans).remove(&id)
    }

    /// Every `/v1/messages` and `count_tokens` arrival since the last call.
    pub fn take_arrivals(&self) -> Vec<Instant> {
        std::mem::take(&mut *lock(&self.arrivals))
    }

    pub fn unexpected(&self) -> BTreeMap<String, usize> {
        lock(&self.unexpected).clone()
    }

    pub fn untagged(&self) -> u64 {
        self.untagged.load(Ordering::Relaxed)
    }

    fn record(&self, tag: Option<u64>, span: Span) {
        if let Some(id) = tag {
            lock(&self.spans).insert(id, span);
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Bind the stub and serve it until the process exits. Returns its port.
pub async fn start(stub: Arc<Stub>) -> anyhow::Result<u16> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let app = Router::new().fallback(handle).with_state(stub);
    tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            eprintln!("[load-test] stub stopped: {err}");
        }
    });
    Ok(port)
}

async fn handle(
    State(stub): State<Arc<Stub>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let start = Instant::now();
    let tag = headers
        .get(TAG_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let path = uri.path();
    let n = stub.counter.fetch_add(1, Ordering::Relaxed);
    match (method.as_str(), path) {
        ("POST", "/v1/messages") => {
            note_arrival(&stub, tag, start);
            let streamed = serde_json::from_slice::<Value>(&body)
                .ok()
                .and_then(|v| v.get("stream").and_then(Value::as_bool))
                .unwrap_or(false);
            if streamed {
                streamed_reply(stub, tag, start, n)
            } else {
                tokio::time::sleep(stub.reply_time).await;
                let reply = json_response(StatusCode::OK, &message_json(n), n);
                stub.record(
                    tag,
                    Span {
                        start,
                        end: Instant::now(),
                    },
                );
                reply
            }
        }
        ("POST", "/v1/messages/count_tokens") => {
            note_arrival(&stub, tag, start);
            let reply = json_response(StatusCode::OK, &json!({"input_tokens": INPUT_TOKENS}), n);
            stub.record(
                tag,
                Span {
                    start,
                    end: Instant::now(),
                },
            );
            reply
        }
        ("GET", "/api/oauth/profile") => json_response(StatusCode::OK, &profile_json(), n),
        _ => {
            *lock(&stub.unexpected)
                .entry(format!("{method} {path}"))
                .or_default() += 1;
            json_response(
                StatusCode::NOT_FOUND,
                &json!({"type": "error", "error": {"type": "not_found_error", "message": "stub"}}),
                n,
            )
        }
    }
}

fn note_arrival(stub: &Stub, tag: Option<u64>, at: Instant) {
    if tag.is_none() {
        stub.untagged.fetch_add(1, Ordering::Relaxed);
    }
    lock(&stub.arrivals).push(at);
}

fn streamed_reply(stub: Arc<Stub>, tag: Option<u64>, start: Instant, n: u64) -> Response {
    let chunks = sse_chunks(n);
    let first_byte = stub.reply_time.mul_f64(0.4);
    let between = if chunks.len() > 1 {
        stub.reply_time.mul_f64(0.6) / (chunks.len() as u32 - 1)
    } else {
        Duration::ZERO
    };
    let total = chunks.len();
    let stream = futures::stream::unfold(
        (chunks.into_iter().enumerate(), stub),
        move |(mut chunks, stub)| async move {
            let (i, chunk) = chunks.next()?;
            let wait = if i == 0 { first_byte } else { between };
            if !wait.is_zero() {
                tokio::time::sleep(wait).await;
            }
            if i + 1 == total {
                stub.record(
                    tag,
                    Span {
                        start,
                        end: Instant::now(),
                    },
                );
            }
            Some((Ok::<Bytes, Infallible>(chunk), (chunks, stub)))
        },
    );
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = StatusCode::OK;
    let headers = response.headers_mut();
    headers.insert(
        "content-type",
        HeaderValue::from_static("text/event-stream"),
    );
    rate_limit_headers(headers, n);
    response
}

fn json_response(status: StatusCode, value: &Value, n: u64) -> Response {
    let mut response = Response::new(Body::from(value.to_string()));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    rate_limit_headers(headers, n);
    response
}

/// The unified rate-limit headers a live Anthropic reply carries, at a quiet 12% / 30%.
fn rate_limit_headers(headers: &mut HeaderMap, n: u64) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let five_hour_reset = (now + 3 * 3_600).to_string();
    let week_reset = (now + 4 * 86_400).to_string();
    let pairs: [(&'static str, String); 9] = [
        ("anthropic-ratelimit-unified-status", "allowed".into()),
        ("anthropic-ratelimit-unified-5h-status", "allowed".into()),
        ("anthropic-ratelimit-unified-5h-utilization", "0.12".into()),
        (
            "anthropic-ratelimit-unified-5h-reset",
            five_hour_reset.clone(),
        ),
        ("anthropic-ratelimit-unified-7d-status", "allowed".into()),
        ("anthropic-ratelimit-unified-7d-utilization", "0.30".into()),
        ("anthropic-ratelimit-unified-7d-reset", week_reset),
        (
            "anthropic-ratelimit-unified-representative-claim",
            "five_hour".into(),
        ),
        ("request-id", format!("req_fake_{n:08}")),
    ];
    for (name, value) in pairs {
        if let Ok(value) = HeaderValue::from_str(&value) {
            headers.insert(name, value);
        }
    }
    if let Ok(value) = HeaderValue::from_str(&five_hour_reset) {
        headers.insert("anthropic-ratelimit-unified-reset", value);
    }
}

/// A fake thinking signature: long, like a real one, and unique per reply so every reply
/// mints new bound tokens.
pub fn signature(n: u64) -> String {
    format!("sig_fake_{n:012}_{}", "A".repeat(320))
}

pub fn message_id(n: u64) -> String {
    format!("msg_fake_{n:012}")
}

fn usage() -> Value {
    json!({
        "input_tokens": INPUT_TOKENS,
        "cache_creation_input_tokens": 0,
        "cache_read_input_tokens": CACHE_READ_TOKENS,
        "cache_creation": {"ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 0},
        "output_tokens": OUTPUT_TOKENS,
        "service_tier": "standard"
    })
}

/// A buffered `/v1/messages` reply: a signed thinking block and a text block.
pub fn message_json(n: u64) -> Value {
    json!({
        "id": message_id(n),
        "type": "message",
        "role": "assistant",
        "model": "claude-opus-5-5",
        "content": [
            {"type": "thinking", "thinking": "Considering the request.", "signature": signature(n)},
            {"type": "text", "text": "Done."}
        ],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": usage()
    })
}

/// A streamed reply as SSE chunks, in the order and shape Anthropic sends them: the message,
/// a signed thinking block, a text block in three deltas, then the closing usage.
pub fn sse_chunks(n: u64) -> Vec<Bytes> {
    let event = |name: &str, data: Value| Bytes::from(format!("event: {name}\ndata: {data}\n\n"));
    let mut start_usage = usage();
    start_usage["output_tokens"] = json!(1);
    let mut chunks = vec![
        event(
            "message_start",
            json!({"type": "message_start", "message": {
                "id": message_id(n), "type": "message", "role": "assistant",
                "model": "claude-opus-5-5", "content": [], "stop_reason": null,
                "stop_sequence": null, "usage": start_usage}}),
        ),
        event(
            "content_block_start",
            json!({"type": "content_block_start", "index": 0,
                   "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
        ),
        event(
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "thinking_delta", "thinking": "Considering the request."}}),
        ),
        event(
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "signature_delta", "signature": signature(n)}}),
        ),
        event(
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 0}),
        ),
        event(
            "content_block_start",
            json!({"type": "content_block_start", "index": 1,
                   "content_block": {"type": "text", "text": ""}}),
        ),
    ];
    for word in ["Working ", "on it, ", "done."] {
        chunks.push(event(
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 1,
                   "delta": {"type": "text_delta", "text": word}}),
        ));
    }
    chunks.push(event(
        "content_block_stop",
        json!({"type": "content_block_stop", "index": 1}),
    ));
    chunks.push(event(
        "message_delta",
        json!({"type": "message_delta",
               "delta": {"stop_reason": "end_turn", "stop_sequence": null},
               "usage": {"output_tokens": OUTPUT_TOKENS}}),
    ));
    chunks.push(event("message_stop", json!({"type": "message_stop"})));
    chunks
}

/// The OAuth profile, answered in case anything in the proxy asks for one. Fake values only.
fn profile_json() -> Value {
    json!({
        "account": {"uuid": "11111111-1111-1111-1111-111111111111",
                    "email": "alice@example.com", "display_name": "alice"},
        "organization": {"uuid": "22222222-2222-2222-2222-222222222222",
                         "name": "Example Org", "organization_type": "claude_max",
                         "rate_limit_tier": "default_claude_max_20x"}
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn events(chunks: &[Bytes]) -> Vec<(String, Value)> {
        chunks
            .iter()
            .map(|chunk| {
                let text = std::str::from_utf8(chunk).expect("utf-8");
                let (head, data) = text
                    .trim_end()
                    .split_once("\ndata: ")
                    .expect("an event line and a data line");
                let name = head.strip_prefix("event: ").expect("event: prefix");
                (
                    name.to_string(),
                    serde_json::from_str(data).expect("json data"),
                )
            })
            .collect()
    }

    #[test]
    fn a_streamed_reply_is_well_formed_sse_carrying_usage_and_bound_tokens() {
        let chunks = sse_chunks(7);
        assert!(
            chunks.iter().all(|c| c.ends_with(b"\n\n")),
            "each event ends a frame"
        );
        let evs = events(&chunks);
        assert_eq!(evs.first().map(|e| e.0.as_str()), Some("message_start"));
        assert_eq!(evs.last().map(|e| e.0.as_str()), Some("message_stop"));
        for (name, data) in &evs {
            assert_eq!(data["type"], json!(name), "event name matches its type");
        }
        assert_eq!(evs[0].1["message"]["id"], json!(message_id(7)));
        assert_eq!(
            evs[0].1["message"]["usage"]["cache_read_input_tokens"],
            json!(CACHE_READ_TOKENS)
        );
        let signature_delta = evs
            .iter()
            .find(|(_, d)| d["delta"]["type"] == "signature_delta")
            .expect("a signature delta");
        assert_eq!(signature_delta.1["delta"]["signature"], json!(signature(7)));
        let closing = evs
            .iter()
            .find(|(name, _)| name == "message_delta")
            .expect("a message_delta");
        assert_eq!(closing.1["usage"]["output_tokens"], json!(OUTPUT_TOKENS));
    }

    #[test]
    fn a_buffered_reply_carries_the_same_tokens_and_usage() {
        let body = message_json(9);
        assert_eq!(body["id"], json!(message_id(9)));
        assert_eq!(body["content"][0]["signature"], json!(signature(9)));
        assert_eq!(body["usage"]["output_tokens"], json!(OUTPUT_TOKENS));
        assert_ne!(signature(9), signature(10), "every reply mints a new token");
    }

    #[test]
    fn every_reply_carries_the_unified_rate_limit_headers() {
        let mut headers = HeaderMap::new();
        rate_limit_headers(&mut headers, 3);
        for name in [
            "anthropic-ratelimit-unified-status",
            "anthropic-ratelimit-unified-5h-utilization",
            "anthropic-ratelimit-unified-5h-reset",
            "anthropic-ratelimit-unified-7d-utilization",
            "anthropic-ratelimit-unified-7d-reset",
        ] {
            assert!(headers.contains_key(name), "missing {name}");
        }
        let reset: u64 = headers["anthropic-ratelimit-unified-5h-reset"]
            .to_str()
            .expect("ascii")
            .parse()
            .expect("whole seconds");
        assert!(
            reset > 1_700_000_000,
            "an epoch in seconds, not ms: {reset}"
        );
    }
}
