//! A `tcr status` wire value a test can parse into a `StatusPayload`.

/// The wire form of a status read whose pool lists `codex_rows` Codex accounts, with the
/// `mitm` key set only when given (a server older than it leaves it out). A caller adds any
/// further key it needs, then parses the value with `serde_json::from_value`.
pub fn codex_status_wire(mitm: Option<serde_json::Value>, codex_rows: usize) -> serde_json::Value {
    let accounts: Vec<serde_json::Value> = (0..codex_rows)
        .map(|i| {
            serde_json::json!({
                "name": format!("codex{i}@example.com/codex"), "provider": "codex",
                "priority": i, "status": "active", "disabled": false, "requests": 0,
                "inputTokens": 0, "outputTokens": 0, "cacheReadTokens": 0,
                "cacheCreationTokens": 0, "probeStatus": "ok", "quotaState": "normal",
                "gate": "ok", "threshold": 0.9,
            })
        })
        .collect();
    let mut wire = serde_json::json!({
        "kind": crate::status::STATUS_KIND,
        "accounts": accounts,
        "build": {"sha": "abc1234"},
    });
    if let Some(mitm) = mitm {
        wire["mitm"] = mitm;
    }
    wire
}
