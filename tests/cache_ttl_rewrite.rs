//! The `ttl: 1h` rewrite, measured at the wire: what the UPSTREAM receives, not
//! what the proxy intended. A fake upstream records every body; the proxy in
//! front of it is the real `mitm::serve` with a one-account config. No real
//! account data anywhere here: this repository is public.

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::response::Response;
use axum::routing::any;

/// Every request body the fake upstream saw, in arrival order.
type Bodies = Arc<Mutex<Vec<Vec<u8>>>>;

async fn spawn_upstream_recording(seen: Bodies) -> String {
    let app = Router::new().fallback(any(move |req: axum::extract::Request| {
        let seen = Arc::clone(&seen);
        async move {
            let body = axum::body::to_bytes(req.into_body(), 4 * 1024 * 1024)
                .await
                .expect("read the forwarded body");
            seen.lock()
                .expect("the body log is never poisoned")
                .push(body.to_vec());
            Response::builder()
                .status(200)
                .header("content-type", "application/json")
                .body(Body::from(br#"{"type":"message"}"#.to_vec()))
                .expect("build the canned answer")
        }
    }));
    let listening = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the fake upstream");
    let addr = listening.local_addr().expect("upstream addr");
    tokio::spawn(async move {
        let _ = axum::serve(listening, app).await;
    });
    format!("http://{addr}")
}

/// A manager with ONE fake account pointed at `upstream`; `extra` is spliced
/// into the top-level config object (the unmodelled flags live there).
fn manager(upstream: &str, extra: &str) -> Arc<tcr::manager::Manager> {
    let config: tcr::config::Config = serde_json::from_str(&format!(
        r#"{{
            "proxy": {{ "port": 0 }},
            "upstream": "{upstream}",
            "quotaProbeSeconds": 0,
            "warmupSeconds": 0,
            {extra}
            "accounts": [
                {{
                    "name": "rewrite-fake",
                    "accessToken": "at-fake-rewrite",
                    "accountUuid": "11111111-1111-1111-1111-111111111111",
                    "orgUuid": "22222222-2222-2222-2222-222222222222"
                }}
            ]
        }}"#
    ))
    .expect("the inline config parses");
    tcr::manager::Manager::with_live_refresher(config, None)
}

async fn spawn_proxy(manager: Arc<tcr::manager::Manager>) -> String {
    let listening = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the proxy");
    let addr = listening.local_addr().expect("proxy addr");
    tokio::spawn(async move {
        tcr::mitm::serve(listening, manager, None).await;
    });
    format!("http://{addr}")
}

/// A request shaped like Claude Code's: a system breakpoint on the default
/// window, a tool with an explicit `5m`, and a message block with none.
const CLIENT_BODY: &str = r#"{"model":"claude-opus-5-5","max_tokens":1,"system":[{"type":"text","text":"sys","cache_control":{"type":"ephemeral"}}],"tools":[{"name":"t","input_schema":{"type":"object"},"cache_control":{"type":"ephemeral","ttl":"5m"}}],"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}"#;

async fn send_through(proxy: &str) -> u16 {
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("the local client builds");
    client
        .post(format!("{proxy}/v1/messages"))
        .header("content-type", "application/json")
        .body(CLIENT_BODY)
        .send()
        .await
        .expect("the proxy answered")
        .status()
        .as_u16()
}

fn only_body(seen: &Bodies) -> serde_json::Value {
    let bodies = seen.lock().expect("the body log is never poisoned");
    assert_eq!(bodies.len(), 1, "exactly one request reached upstream");
    serde_json::from_slice(&bodies[0]).expect("upstream received JSON")
}

/// Default config: every breakpoint arrives upstream as `1h`, the block with no
/// breakpoint stays bare, and the rest of the body is intact.
///
/// Watched red with `extend_all_ttls` short-circuited to `None`: the first
/// assertion failed on `"ttl"` being absent.
#[tokio::test]
async fn breakpoints_arrive_upstream_as_1h_by_default() {
    let seen: Bodies = Arc::new(Mutex::new(Vec::new()));
    let upstream = spawn_upstream_recording(Arc::clone(&seen)).await;
    let proxy = spawn_proxy(manager(&upstream, "")).await;

    assert_eq!(send_through(&proxy).await, 200);

    let body = only_body(&seen);
    assert_eq!(body["system"][0]["cache_control"]["ttl"], "1h");
    assert_eq!(
        body["tools"][0]["cache_control"]["ttl"], "1h",
        "an explicit 5m becomes 1h"
    );
    assert!(
        body["messages"][0]["content"][0]
            .get("cache_control")
            .is_none()
    );
    assert_eq!(body["model"], "claude-opus-5-5");
    assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
}

/// `"cacheTtlRewrite": false` forwards the client's bytes exactly as sent.
#[tokio::test]
async fn rewrite_off_forwards_the_body_byte_for_byte() {
    let seen: Bodies = Arc::new(Mutex::new(Vec::new()));
    let upstream = spawn_upstream_recording(Arc::clone(&seen)).await;
    let proxy = spawn_proxy(manager(&upstream, r#""cacheTtlRewrite": false,"#)).await;

    assert_eq!(send_through(&proxy).await, 200);

    let bodies = seen.lock().expect("the body log is never poisoned");
    assert_eq!(bodies.len(), 1);
    assert_eq!(
        std::str::from_utf8(&bodies[0]).expect("utf-8"),
        CLIENT_BODY,
        "with the rewrite off, upstream sees the client's own bytes"
    );
}

async fn send_body(proxy: &str, body: Vec<u8>) -> u16 {
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("the local client builds");
    client
        .post(format!("{proxy}/v1/messages"))
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .expect("the proxy answered")
        .status()
        .as_u16()
}

/// A long Claude Code shaped body of about `target` bytes: `metadata.user_id` naming
/// `account_uuid`, written LAST so every breakpoint splice lands before it, and every
/// breakpoint spelled `ttl` (`None` leaves it to the default window).
fn long_body(target: usize, account_uuid: &str, ttl: Option<&str>) -> Vec<u8> {
    let cache_control = match ttl {
        Some(ttl) => format!(r#"{{"type":"ephemeral","ttl":"{ttl}"}}"#),
        None => r#"{"type":"ephemeral"}"#.to_string(),
    };
    let user_id = serde_json::to_string(&format!(
        r#"{{"device_id":"d1","account_uuid":"{account_uuid}","session_id":"33333333-3333-3333-3333-333333333333"}}"#
    ))
    .expect("a JSON string");
    let mut messages = Vec::new();
    let mut size = 0usize;
    let mut n = 0usize;
    while size < target {
        let turn = format!(
            r#"{{"role":"assistant","content":[{{"type":"thinking","thinking":"step {n}","signature":"sig-{n}"}},{{"type":"tool_use","id":"toolu_{n}","name":"Bash","input":{{"command":"ls"}}}}]}},{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"toolu_{n}","content":"{}","cache_control":{cache_control}}}]}}"#,
            "out \\\"put\\\"\\n".repeat(200)
        );
        size += turn.len();
        messages.push(turn);
        n += 1;
    }
    format!(
        r#"{{"model":"claude-opus-5-5","max_tokens":1,"system":[{{"type":"text","text":"sys","cache_control":{cache_control}}}],"tools":[{{"name":"t","input_schema":{{"type":"object"}},"cache_control":{cache_control}}}],"messages":[{}],"metadata":{{"user_id":{user_id}}}}}"#,
        messages.join(",")
    )
    .into_bytes()
}

/// The configured account's uuid in [`manager`].
const SERVING_UUID: &str = "11111111-1111-1111-1111-111111111111";

/// With the rewrite on (the default) and nothing for it or the uuid patch to change, a
/// multi-megabyte body reaches upstream as the client's own bytes.
///
/// Watched red with a one-byte splice planted into every body: upstream saw one byte more.
#[tokio::test]
async fn an_unchanged_long_body_reaches_upstream_byte_for_byte() {
    let seen: Bodies = Arc::new(Mutex::new(Vec::new()));
    let upstream = spawn_upstream_recording(Arc::clone(&seen)).await;
    let proxy = spawn_proxy(manager(&upstream, "")).await;
    let body = long_body(2 * 1024 * 1024, SERVING_UUID, Some("1h"));
    assert!(body.len() > 2 * 1024 * 1024);

    assert_eq!(send_body(&proxy, body.clone()).await, 200);

    let bodies = seen.lock().expect("the body log is never poisoned");
    assert_eq!(bodies.len(), 1);
    assert!(
        bodies[0] == body,
        "upstream saw {} bytes, the client sent {}",
        bodies[0].len(),
        body.len()
    );
}

/// When the rewrite DOES change the body, the uuid patch still lands on `metadata.user_id`
/// after every splice before it: upstream sees exactly what the two separate steps
/// produced, rewrite then patch.
///
/// Watched red with the offset left in the client's coordinates (`moved_by` returning its
/// input): the patch fell outside `user_id`, failed its own check, and the old uuid went out.
#[tokio::test]
async fn a_rewritten_long_body_gets_its_uuid_patched_after_the_splices() {
    let seen: Bodies = Arc::new(Mutex::new(Vec::new()));
    let upstream = spawn_upstream_recording(Arc::clone(&seen)).await;
    let proxy = spawn_proxy(manager(&upstream, "")).await;
    let body = long_body(1024 * 1024, "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa", None);
    let rewritten = tcr::cache_ttl::extend_all_ttls(&body).expect("breakpoints to extend");
    let expected = tcr::account_uuid::patch_account_uuid(&rewritten, SERVING_UUID).into_owned();
    assert_ne!(expected, rewritten, "control: the patch changes something");

    assert_eq!(send_body(&proxy, body).await, 200);

    let bodies = seen.lock().expect("the body log is never poisoned");
    assert_eq!(bodies.len(), 1);
    assert!(
        bodies[0] == expected,
        "upstream saw {} bytes, expected {}",
        bodies[0].len(),
        expected.len()
    );
}
