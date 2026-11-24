//! Tests for `cli/live_reads.rs`: `tcr sessions` and `tcr routing`.

use super::*;
/// A minimal but REAL status body — it must survive `parse_live_status`,
/// so `kind` has to be the live constant rather than a placeholder.
fn status_body(extra: &str) -> String {
    format!(r#"{{"kind":"{STATUS_KIND}","accounts":[]{extra}}}"#)
}

/// The whole reason `supported` is read off the raw body: an EMPTY
/// `sessions` array is still a server that reported the key. The typed
/// payload cannot tell this case from the one below — both decode to an
/// empty vec — so a test that went through `StatusPayload` would pass
/// while the panel drew the wrong sentence.
#[test]
fn an_empty_sessions_array_is_still_a_supported_server() {
    let body = status_body(r#","sessions":[]"#);
    assert!(payload_carries_sessions(&body));
    let Ok(payload) = parse_live_status(&body) else {
        panic!("a well-formed status body must parse");
    };
    assert!(payload.sessions.is_empty());
}

/// The pre-F1 server: no such key at all.
#[test]
fn a_payload_without_the_sessions_key_is_unsupported() {
    assert!(!payload_carries_sessions(&status_body("")));
}

#[test]
fn a_payload_with_rows_is_supported() {
    let body = status_body(
        r#","sessions":[{"sessionId":"11111111-1111-1111-1111-111111111111","firstSeenMs":1,"lastSeenMs":2}]"#,
    );
    assert!(payload_carries_sessions(&body));
    let Ok(payload) = parse_live_status(&body) else {
        panic!("a well-formed status body must parse");
    };
    assert_eq!(payload.sessions.len(), 1);
}

/// Not-JSON is not a channel. `false`, never a panic and never `true`.
#[test]
fn a_non_json_body_carries_no_sessions() {
    assert!(!payload_carries_sessions("not json at all"));
}

/// What `tcr sessions` prints for a Claude-only fleet, pinned before the sessions wire
/// learned about Codex: every later change must leave these bytes exactly as they are. The
/// rows are built from JSON holding only the keys a Claude session has always carried, so
/// the golden cannot lean on a field added after it. The second row's model has no price in
/// this build and prints `costUsd=0.0000`. That zero is a known defect, kept on purpose:
/// fixing it would change what a Claude-only fleet prints.
#[test]
fn claude_only_sessions_text_is_unchanged() {
    let rows: Vec<tcr_status_wire::SessionRow> = serde_json::from_value(serde_json::json!([
        {
            "sessionId": "a1f3c2d4-0000-4000-8000-000000000001",
            "account": "alice@example.com",
            "model": "claude-opus-5",
            "firstSeenMs": 1_767_223_200_000_i64,
            "lastSeenMs": 1_767_225_597_000_i64,
            "requests": 24,
            "inputTokens": 180_000,
            "outputTokens": 9_200,
            "cacheReadTokens": 160_000,
            "cacheCreationTokens": 12_000,
            "tools": { "calls": 12 },
            "costUsd": 1.2034,
            "cacheTtlSecs": 3600
        },
        {
            "sessionId": "22222222-2222-4222-8222-222222222222",
            "account": "carol@example.com",
            "model": "claude-opus-4-5",
            "firstSeenMs": 1_767_225_000_000_i64,
            "lastSeenMs": 1_767_225_060_000_i64,
            "requests": 1,
            "inputTokens": 10,
            "outputTokens": 2
        },
        {
            "sessionId": "33333333-3333-4333-8333-333333333333",
            "firstSeenMs": 1_767_225_000_000_i64,
            "lastSeenMs": 1_767_225_000_000_i64,
            "requests": 1
        }
    ]))
    .expect("session rows in today's shape");
    assert_eq!(
        render_sessions_text(true, &rows),
        concat!(
            "supported=true sessions=3\n",
            "session=a1f3c2d4-0000-4000-8000-000000000001 account=alice@example.com model=claude-opus-5 requests=24 inputTokens=180000 outputTokens=9200 cacheReadTokens=160000 toolCalls=12 costUsd=1.2034\n",
            "session=22222222-2222-4222-8222-222222222222 account=carol@example.com model=claude-opus-4-5 requests=1 inputTokens=10 outputTokens=2 cacheReadTokens=0 toolCalls=0 costUsd=0.0000\n",
            "session=33333333-3333-4333-8333-333333333333 account=- model=- requests=1 inputTokens=0 outputTokens=0 cacheReadTokens=0 toolCalls=0 costUsd=0.0000\n",
        )
    );
    assert_eq!(
        render_sessions_text(false, &[]),
        "supported=false sessions=0\n"
    );
}

/// A Codex session's line names its provider after the account and never prints a cost
/// nobody measured: `costUsd=n/a` while no request was priced, including before its first
/// response finished; the priced part and the count it leaves out when only some were; the
/// plain figure when all were. Reasoning tokens print when there are any.
#[test]
fn a_codex_session_line_carries_provider_and_its_unpriced_figures() {
    let codex =
        |id: &str, cost: f64, unpriced: u64, reasoning: u64| -> tcr_status_wire::SessionRow {
            serde_json::from_value(serde_json::json!({
                "sessionId": id,
                "account": "bob@example.com/codex",
                "provider": "codex",
                "model": "gpt-5.1-codex",
                "firstSeenMs": 1_767_224_880_000_i64,
                "lastSeenMs": 1_767_225_599_000_i64,
                "requests": 7,
                "inputTokens": 95_000,
                "outputTokens": 4_100,
                "cacheReadTokens": 80_000,
                "tools": { "calls": 3 },
                "costUsd": cost,
                "unpricedRequests": unpriced,
                "reasoningTokens": reasoning
            }))
            .expect("a Codex session row")
        };
    let rows = [
        codex("019a3c5e-0000-7000-8000-000000000002", 0.0, 7, 1_200),
        codex("44444444-4444-4444-8444-444444444444", 0.42, 3, 0),
        codex("55555555-5555-4555-8555-555555555555", 1.5, 0, 0),
        codex("66666666-6666-4666-8666-666666666666", 0.0, 0, 0),
    ];
    let tail = "account=bob@example.com/codex provider=codex model=gpt-5.1-codex requests=7 inputTokens=95000 outputTokens=4100 cacheReadTokens=80000 toolCalls=3";
    assert_eq!(
        render_sessions_text(true, &rows),
        format!(
            "supported=true sessions=4\n\
                 session=019a3c5e-0000-7000-8000-000000000002 {tail} costUsd=n/a reasoningTokens=1200\n\
                 session=44444444-4444-4444-8444-444444444444 {tail} costUsd=0.4200 unpricedRequests=3\n\
                 session=55555555-5555-4555-8555-555555555555 {tail} costUsd=1.5000\n\
                 session=66666666-6666-4666-8666-666666666666 {tail} costUsd=n/a\n"
        )
    );
}

/// The document shape the Swift poller decodes. Written as literal text
/// rather than round-tripped through the same `json!` macro that produced
/// it — an assertion built from the code under test proves only that the
/// code equals itself.
#[test]
fn sessions_json_is_an_object_with_supported_and_rows() {
    let row = tcr_status_wire::SessionRow {
        session_id: "11111111-1111-1111-1111-111111111111".to_string(),
        account: Some("alice@example.com".to_string()),
        model: Some("claude-sonnet-5".to_string()),
        first_seen_ms: 1_700_000_000_000,
        last_seen_ms: 1_700_000_060_000,
        requests: 3,
        input_tokens: 400,
        output_tokens: 50,
        cache_read_tokens: 900,
        cache_creation_tokens: 25,
        tools: tcr_status_wire::SessionToolsRow::default(),
        req_per_minute: vec![0, 1, 2],
        cost_usd: 0.5,
        cache_ttl_secs: Some(3600),
        provider: None,
        unpriced_requests: 0,
        reasoning_tokens: 0,
    };
    let rendered = render_sessions_json(true, std::slice::from_ref(&row), None, None);
    let value: serde_json::Value = serde_json::from_str(&rendered).expect("valid JSON");
    assert_eq!(value["supported"], serde_json::json!(true));
    // camelCase keys, exactly as the Swift `Session` decoder names them
    // (`FleetStatus.swift`'s `CodingKeys`) — a `rename_all` regression on
    // the wire type fails here rather than on a screen.
    assert_eq!(
        value["sessions"][0]["sessionId"],
        serde_json::json!("11111111-1111-1111-1111-111111111111")
    );
    assert_eq!(
        value["sessions"][0]["cacheReadTokens"],
        serde_json::json!(900)
    );
    assert_eq!(
        value["sessions"][0]["reqPerMinute"],
        serde_json::json!([0, 1, 2])
    );
    assert_eq!(value["sessions"][0]["costUsd"], serde_json::json!(0.5));
    assert_eq!(
        value["sessions"][0]["cacheTtlSecs"],
        serde_json::json!(3600)
    );

    // The unsupported document still carries both keys, so the client
    // never has to tell "absent" from "false".
    let empty = render_sessions_json(false, &[], None, None);
    let value: serde_json::Value = serde_json::from_str(&empty).expect("valid JSON");
    assert_eq!(value["supported"], serde_json::json!(false));
    assert_eq!(value["sessions"], serde_json::json!([]));
    assert!(
        value.get("heat").is_none(),
        "no heat key without a reading: {value}"
    );
}

/// The server's heat reading crosses `tcr sessions --json` whole, under the
/// camelCase keys `FleetStatus.swift`'s `HeatReading` decodes.
#[test]
fn sessions_json_carries_the_heat_reading() {
    let heat = tcr_status_wire::HeatRow {
        cpu_celsius: Some(61.0),
        fans: vec![tcr_status_wire::FanRow {
            index: 1,
            actual_rpm: 5013.0,
            min_rpm: 1350.0,
            max_rpm: 5777.0,
            manual: false,
            target_rpm: None,
        }],
        ftst: None,
    };
    let rendered = render_sessions_json(true, &[], Some(&heat), None);
    let value: serde_json::Value = serde_json::from_str(&rendered).expect("valid JSON");
    assert_eq!(
        value["heat"],
        serde_json::json!({
            "cpuCelsius": 61.0,
            "fans": [{"index": 1, "actualRpm": 5013.0, "minRpm": 1350.0,
                      "maxRpm": 5777.0, "manual": false}],
        })
    );
    assert_eq!(value["sessions"], serde_json::json!([]));
    assert!(
        value.get("fanControl").is_none(),
        "no key without a controller"
    );
}

/// `fanControl` crosses `tcr sessions --json` whole, beside `heat`, exactly as the
/// committed contract sample spells it: the object TcrBar's popover decodes.
#[test]
fn fan_status_crosses_sessions_json_as_the_contract_says() {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fan-control-contract.json");
    let samples: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    let sample = samples["holdingFixed5000"].clone();
    let row: tcr_status_wire::FanControlRow = serde_json::from_value(sample.clone()).unwrap();
    let rendered = render_sessions_json(true, &[], None, Some(&row));
    let value: serde_json::Value = serde_json::from_str(&rendered).expect("valid JSON");
    assert_eq!(value["fanControl"], sample);
}

/// **`tcr routing --json` exits 3 only when nothing listens**: a refused connection is
/// the one way no proxy answered. Silence, and a client that could not be built, are 4,
/// with every answer that is not a readable schema-1 document.
#[test]
fn routing_exits_3_only_when_nothing_listens() {
    assert_eq!(RoutingReadError::from(Unreached::Refused).exit_code(), 3);
    for unreached in [
        Unreached::Silent("the server did not answer within 5s".to_string()),
        Unreached::Unbuilt("http client: no TLS".to_string()),
    ] {
        assert_eq!(RoutingReadError::from(unreached).exit_code(), 4);
    }
    assert_eq!(
        RoutingReadError::Unreadable("HTTP 500".to_string()).exit_code(),
        4
    );
}
#[tokio::test]
async fn reviewer_routing_rejects_an_http_redirect() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target_port = target.local_addr().unwrap().port();
    let origin = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin_port = origin.local_addr().unwrap().port();
    let origin_task = tokio::spawn(async move {
        let (mut stream, _) = origin.accept().await.unwrap();
        let mut request = [0u8; 4096];
        let _ = stream.read(&mut request).await.unwrap();
        stream.write_all(format!("HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{target_port}/unrelated-service\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
    });
    let target_task = tokio::spawn(async move {
        if let Ok(Ok((mut stream, _))) =
            tokio::time::timeout(Duration::from_secs(2), target.accept()).await
        {
            let mut request = [0u8; 4096];
            let n = stream.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..n]).to_string();
            let body = r#"{"schema":1,"generatedAt":"2026-10-26T00:00:00Z","build":{"sha":"fake","dirty":false,"builtAt":"2026-10-26T00:00:00Z"},"providers":[],"forecast":null}"#;
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            Some(request)
        } else {
            None
        }
    });
    let mut config = Config::fixture(vec![]);
    config.proxy.port = origin_port;
    config.proxy.api_key = Some("fake-routing-key".to_string());
    let result = fetch_routing_body(&config).await;
    origin_task.await.unwrap();
    let forwarded = target_task.await.unwrap();
    assert!(
        result.is_err(),
        "HTTP 302 must be unreadable, redirected answer was accepted; request at second service: {forwarded:?}"
    );
    let error = result.unwrap_err();
    assert_eq!(error.exit_code(), 4);
    assert!(
        forwarded.is_none(),
        "a redirect target must receive no request or API key"
    );
}
