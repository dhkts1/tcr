use super::*;

// --- `--non-interactive` (the TcrBar in-app login) ----------------------

/// The whole wire contract `LoginSession.swift` parses, pinned as literal
/// text. Hardcoded rather than built from the same `serde_json::json!`
/// the production code uses — an assertion written that way passes no
/// matter what the key names become, which is the one thing this test
/// exists to stop.
#[test]
fn every_event_renders_the_exact_line_the_app_parses() {
    // Key ORDER is serde's (alphabetical) and no JSON parser cares, so
    // each object is compared as a whole parsed value: that still fails on
    // a renamed key, a dropped key or an extra one, which is what the
    // Swift side would break on.
    let parsed = |line: String| -> serde_json::Value {
        serde_json::from_str(&line).expect("every event line is one JSON object")
    };
    assert_eq!(
        parsed(
            LoginEvent::Browser {
                url: "https://claude.ai/oauth/authorize?code=true&state=abc"
            }
            .line()
        ),
        serde_json::json!({
            "event": "browser",
            "url": "https://claude.ai/oauth/authorize?code=true&state=abc"
        })
    );
    assert_eq!(LoginEvent::Waiting.line(), r#"{"event":"waiting"}"#);
    assert_eq!(
        parsed(
            LoginEvent::Saved {
                account: "alice@example.com"
            }
            .line()
        ),
        serde_json::json!({ "event": "saved", "account": "alice@example.com" })
    );
    assert_eq!(
        parsed(
            LoginEvent::Error {
                reason: "the proxy on :3456 rejected the api-key"
            }
            .line()
        ),
        serde_json::json!({
            "event": "error",
            "reason": "the proxy on :3456 rejected the api-key"
        })
    );
}

/// The three Claude lines, byte for byte. A Codex login adds keys to its own lines
/// (`opened`, `provider`, `plan`, `code`), and none of them may reach a Claude line, which
/// TcrBar and every older parser read exactly as they read them before Codex accounts.
///
/// Watched red: rendering `Saved` through the Codex arm adds `provider` to alice's line.
#[test]
fn claude_saved_event_is_unchanged() {
    assert_eq!(
        LoginEvent::Saved {
            account: "alice@example.com"
        }
        .line(),
        r#"{"account":"alice@example.com","event":"saved"}"#
    );
    assert_eq!(
        LoginEvent::Browser {
            url: "https://claude.ai/oauth/authorize?code=true"
        }
        .line(),
        r#"{"event":"browser","url":"https://claude.ai/oauth/authorize?code=true"}"#
    );
    assert_eq!(
        LoginEvent::Error { reason: "x" }.line(),
        r#"{"event":"error","reason":"x"}"#
    );
}

/// The Codex events, against the fixture TcrBar's decoder is tested on
/// (`apps/macos/Tests/Fixtures/codex-login-events.jsonl`): producer and consumer are pinned to
/// one file, so neither can drift from the other. Compared as parsed values, because key
/// order is serde's and no parser cares.
///
/// Watched red: one word of the first-account notice changed fails on fixture line 3.
#[test]
fn codex_login_events_match_the_fixture() {
    let fixture = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("apps/macos/Tests/Fixtures/codex-login-events.jsonl"),
    )
    .expect("the committed fixture");
    let want: Vec<serde_json::Value> = fixture
        .lines()
        .map(|line| serde_json::from_str(line).expect("each fixture line is one JSON object"))
        .collect();
    assert_eq!(want.len(), 5, "{fixture}");
    let produced = [
        LoginEvent::BrowserOpened {
            url: "https://auth.openai.com/oauth/authorize?client_id=app_EXAMPLE&state=EXAMPLE",
        }
        .line(),
        LoginEvent::Waiting.line(),
        LoginEvent::Notice {
            text: CODEX_FIRST_ACCOUNT_NOTICE,
        }
        .line(),
        LoginEvent::SavedCodex {
            account: "bob@example.com",
            plan: Some("ChatGPT Pro"),
        }
        .line(),
        codex_identity_refusal("carol@example.com", "erin@example.com/codex").line(),
    ];
    for (index, (line, want)) in produced.iter().zip(&want).enumerate() {
        let got: serde_json::Value = serde_json::from_str(line).expect("one JSON object");
        assert_eq!(&got, want, "fixture line {}", index + 1);
    }
    assert!(
        !LoginEvent::SavedCodex {
            account: "bob@example.com",
            plan: None
        }
        .line()
        .contains("plan"),
        "no plan key when the plan is unknown"
    );
}

/// A refusal's machine line carries its code and its own reason, without the context a
/// caller wrapped it in; any other failure keeps today's uncoded line, its whole chain
/// flattened onto it.
///
/// Watched red: rendering every error through the plain arm loses the code and prefixes
/// the context.
#[test]
fn a_refusal_keeps_its_code_and_its_reason_verbatim() {
    let refused = anyhow::Error::from(LoginRefusal {
        code: "codex-routed",
        reason: "r".into(),
    })
    .context("importing the Codex login failed");
    assert_eq!(
        machine_error_line(&refused),
        (
            r#"{"code":"codex-routed","event":"error","reason":"r"}"#.to_string(),
            "r".to_string()
        )
    );
    let plain = anyhow!("boom").context("OAuth login failed");
    assert_eq!(
        machine_error_line(&plain),
        (
            r#"{"event":"error","reason":"OAuth login failed: boom"}"#.to_string(),
            "OAuth login failed: boom".to_string()
        )
    );
}

/// A reason carrying a quote and a newline still leaves ONE parseable
/// line. Hand-built JSON is what makes a message like this an unparseable
/// line at the worst possible moment, so the escaping is asserted rather
/// than assumed.
#[test]
fn an_event_line_survives_quotes_and_newlines_in_its_payload() {
    let line = LoginEvent::Error {
        reason: "config \"accounts\"\nis not an array",
    }
    .line();
    assert!(
        !line.contains('\n'),
        "one event is one physical line: {line}"
    );
    let parsed: serde_json::Value = serde_json::from_str(&line).expect("must stay parseable");
    assert_eq!(parsed["reason"], "config \"accounts\"\nis not an array");
}

/// `one_line_reason` flattens an `anyhow` chain AND any newline inside a
/// single message, because both reach a GUI label that has one line to
/// render them in.
#[test]
fn one_line_reason_flattens_the_whole_chain_onto_one_line() {
    let error = anyhow!("the proxy on :3456 answered\nbut not usably (timeout)")
        .context("OAuth login failed");
    let reason = one_line_reason(&error);
    assert_eq!(
        reason,
        "OAuth login failed: the proxy on :3456 answered but not usably (timeout)"
    );
}
