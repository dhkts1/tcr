//! A long Claude Code conversation, generated, for the big-body phases: the shape a real
//! session sends after hours of work (a system prompt with a cache breakpoint, a tool list,
//! hundreds of turns of thinking, tool calls and tool output, a breakpoint on the newest
//! message), at a chosen size. Every word is generated; nothing is copied from a real session.

use serde_json::{json, Value};

use crate::stats::Rng;

/// Words the generated text is drawn from.
const WORDS: [&str; 24] = [
    "the", "proxy", "reads", "each", "request", "once", "and", "forwards", "bytes", "file", "line",
    "error", "test", "build", "cache", "token", "account", "session", "value", "parse", "returns",
    "None", "when", "missing",
];

/// The part of a big body that stays the same from one turn to the next, serialized once:
/// everything before the newest turn, with `messages` left open.
pub struct History {
    /// `{"model":…,"system":…,"tools":…,"messages":[<old turns>` with no closing bracket.
    pub head: String,
}

impl History {
    /// A conversation of about `target_bytes` once serialized, seeded so each session's
    /// history is its own.
    pub fn generate(target_bytes: usize, seed: u64, user_id: &str) -> Self {
        let mut rng = Rng::new(seed);
        let system = json!([
            {"type": "text", "text": "You are an agent for software engineering tasks."},
            {"type": "text", "text": text(&mut rng, 16 * 1024),
             "cache_control": {"type": "ephemeral"}},
        ]);
        let tools: Vec<Value> = (0..24)
            .map(|i| {
                json!({
                    "name": format!("Tool{i}"),
                    "description": text(&mut rng, 1_200),
                    "input_schema": {
                        "type": "object",
                        "properties": {
                            "command": {"type": "string", "description": text(&mut rng, 200)},
                            "timeout": {"type": "number"},
                        },
                        "required": ["command"],
                    },
                })
            })
            .collect();
        let mut head = format!(
            "{{\"model\":\"claude-opus-5-5\",\"max_tokens\":32000,\"metadata\":{},\"system\":{},\"tools\":{},\"messages\":[",
            json!({"user_id": user_id}),
            system,
            Value::Array(tools),
        );
        head.push_str(&json!({"role": "user", "content": "Start the task."}).to_string());
        let mut turn = 0u64;
        while head.len() < target_bytes {
            let id = format!("toolu_{seed:04}{turn:08}");
            let assistant = json!({"role": "assistant", "content": [
                {"type": "thinking", "thinking": text(&mut rng, 900),
                 "signature": format!("sig-history-{seed}-{turn}-{}", "A".repeat(200))},
                {"type": "text", "text": text(&mut rng, 200)},
                {"type": "tool_use", "id": id, "name": "Bash",
                 "input": {"command": format!("cargo test -p tcr {}", text(&mut rng, 60)),
                           "description": text(&mut rng, 40)}},
            ]});
            let user = json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": id, "content": output(&mut rng, 4_096)},
            ]});
            head.push(',');
            head.push_str(&assistant.to_string());
            head.push(',');
            head.push_str(&user.to_string());
            turn += 1;
        }
        Self { head }
    }

    /// One request: the history, then the newest turn (the last reply's signed thinking
    /// echoed, when there is one, and a user message carrying the cache breakpoint).
    pub fn body(&self, last_signature: Option<&str>, stream: bool, count_tokens: bool) -> String {
        let mut out = String::with_capacity(self.head.len() + 512);
        out.push_str(&self.head);
        if let Some(signature) = last_signature {
            out.push(',');
            out.push_str(
                &json!({"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "Considering the request.",
                     "signature": signature},
                    {"type": "text", "text": "Done."},
                ]})
                .to_string(),
            );
        }
        out.push(',');
        out.push_str(
            &json!({"role": "user", "content": [
                {"type": "text", "text": "Continue.", "cache_control": {"type": "ephemeral"}},
            ]})
            .to_string(),
        );
        out.push(']');
        if stream && !count_tokens {
            out.push_str(",\"stream\":true");
        }
        out.push('}');
        out
    }
}

/// About `len` bytes of words.
fn text(rng: &mut Rng, len: usize) -> String {
    let mut out = String::with_capacity(len + 16);
    while out.len() < len {
        out.push_str(WORDS[rng.below(WORDS.len())]);
        out.push(' ');
    }
    out
}

/// About `len` bytes of tool output: lines, quotes and backslashes, which a real tool
/// result is full of and which JSON escapes.
fn output(rng: &mut Rng, len: usize) -> String {
    let mut out = String::with_capacity(len + 64);
    while out.len() < len {
        let n = rng.below(1_000);
        out.push_str(&format!(
            "src/{}.rs:{n}: \"{}\" at C:\\path\\{}\n",
            WORDS[rng.below(WORDS.len())],
            text(rng, 40).trim_end(),
            WORDS[rng.below(WORDS.len())],
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_big_body_is_json_of_about_the_size_asked_with_the_shapes_the_proxy_reads() {
        let history = History::generate(1024 * 1024, 7, "{\"session_id\":\"s\"}");
        let body = history.body(Some("sig-last"), true, false);
        assert!(body.len() >= 1024 * 1024 && body.len() < 1024 * 1024 + 16 * 1024);
        let value: Value = serde_json::from_str(&body).expect("the body is JSON");
        assert_eq!(value["stream"], json!(true));
        let messages = value["messages"].as_array().expect("messages");
        assert!(messages.len() > 200, "{} messages", messages.len());
        let last = messages.last().expect("a newest message");
        assert_eq!(
            last["content"][0]["cache_control"]["type"],
            json!("ephemeral")
        );
        let echoed = &messages[messages.len() - 2];
        assert_eq!(echoed["content"][0]["signature"], json!("sig-last"));
        assert_eq!(
            value["system"][1]["cache_control"]["type"],
            json!("ephemeral")
        );
        assert_eq!(
            value["metadata"]["user_id"],
            json!("{\"session_id\":\"s\"}")
        );
        assert!(
            body.contains("\\\\path") && body.contains("\\n"),
            "escapes present"
        );

        let counted: Value =
            serde_json::from_str(&history.body(None, true, true)).expect("count_tokens body");
        assert!(counted.get("stream").is_none());
    }
}
