//! Tests for `tcr list --v1` in `cli/list_v1.rs`.

use super::*;
/// `tcr list --json` over the config at `path` with a [`CountingProber`], which file is then
/// removed: the document, the exit code, and how many probes reached the prober.
async fn list_v1_counting_probes(path: &Path) -> (String, i32, Arc<AtomicU64>) {
    let calls = Arc::new(AtomicU64::new(0));
    let (document, code) = render_list_v1(
        path,
        Arc::new(CountingProber {
            calls: Arc::clone(&calls),
        }),
    )
    .await;
    fs::remove_file(path).ok();
    (document, code, calls)
}

/// A prober that only counts: `tcr list` must never reach it.
struct CountingProber {
    calls: Arc<AtomicU64>,
}
impl UsageProber for CountingProber {
    fn probe(&self, _credential: crate::records::probe::ProbeCredential) -> ProbeFuture {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Ok(Usage {
                five_hour: crate::records::probe::WindowReading::Read(UsageBucket {
                    utilization: Some(0.5),
                    reset_at_ms: Some(crate::now_ms() + 3_600_000),
                }),
                seven_day: None,
                seven_day_oi: None,
                extra_usage_usd: None,
                codex: None,
                claude_resets: None,
            })
        })
    }
}

/// With no server, `tcr list --json` lists every configured account as
/// `unavailable`, in file order, and the prober sees ZERO calls. Both
/// accounts carry an unexpired token, so a path that probed would reach
/// the prober twice.
#[tokio::test]
async fn list_v1_with_no_server_never_probes_and_lists_every_account_unavailable() {
    let json = TWO_ACCOUNTS.replace(
        "3456",
        &crate::test_support::loopback::unused_port().to_string(),
    );
    let path = write_config("list-v1-no-server", &json);
    let (document, code, calls) = list_v1_counting_probes(&path).await;

    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "tcr list must never probe; a CodexBar poll would become a usage call per account"
    );
    assert_eq!(code, 0);
    let doc: serde_json::Value = serde_json::from_str(&document).expect("one JSON document");
    assert_eq!(doc["schemaVersion"], 1);
    assert!(doc["activeAccountNumber"].is_null());
    assert_eq!(doc["supportsAccountSwitching"], false);
    let accounts = doc["accounts"].as_array().expect("accounts");
    let got: Vec<(i64, &str, &str)> = accounts
        .iter()
        .map(|a| {
            (
                a["number"].as_i64().expect("number"),
                a["email"].as_str().expect("email"),
                a["usageStatus"].as_str().expect("usageStatus"),
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![
            (1, "alice@example.com", "unavailable"),
            (2, "bob@example.com", "unavailable"),
        ]
    );
    assert!(
        accounts.iter().all(|a| a["usage"] == serde_json::json!({})),
        "nothing was measured, so no window is claimed"
    );
}

/// A config that cannot be parsed is the schema-v1 error envelope on
/// stdout with exit 1, because CodexBar parses stdout on any exit code.
#[tokio::test]
async fn list_v1_reports_an_unreadable_config_in_the_error_envelope() {
    let path = write_config("list-v1-bad", "{ not json");
    let (document, code, calls) = list_v1_counting_probes(&path).await;

    assert_eq!(code, 1);
    let doc: serde_json::Value = serde_json::from_str(&document).expect("one JSON document");
    assert_eq!(doc["schemaVersion"], 1);
    assert_eq!(doc["error"]["type"], "config_unreadable");
    assert!(
        doc["error"]["message"]
            .as_str()
            .is_some_and(|m| !m.is_empty())
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

/// **`tcr list --json` never hands CodexBar a Codex account as one of its Claude
/// accounts.** The document is claude-swap's list, which CodexBar reads as Claude
/// accounts and nothing else, and it has no field that could say otherwise: listed,
/// a ChatGPT login would be drawn as a Claude account, under its email, with its
/// Codex windows on Claude bars. The Claude rows keep their file order and number
/// from 1 among themselves.
///
/// Watched red: list every row, and the Codex account takes slot 2 as a second
/// `alice@example.com`.
#[tokio::test]
async fn list_v1_never_lists_a_codex_account() {
    let json = format!(
        r#"{{ "proxy": {{ "port": {} }}, "accounts": [
                 {{ "name": "alice@example.com", "type": "oauth", "accessToken": "at-a",
                    "refreshToken": "rt-a", "expiresAt": 4102444800000 }},
                 {{ "name": "alice@example.com/codex", "provider": "codex", "type": "oauth",
                    "accessToken": "at-c", "refreshToken": "rt-c", "expiresAt": 4102444800000,
                    "accountUuid": "11111111-1111-4111-8111-111111111111", "credentialVersion": 2 }},
                 {{ "name": "bob@example.com", "type": "oauth", "accessToken": "at-b",
                    "refreshToken": "rt-b", "expiresAt": 4102444800000 }}
               ] }}"#,
        crate::test_support::loopback::unused_port()
    );
    let path = write_config("list-v1-codex", &json);
    let (document, code, calls) = list_v1_counting_probes(&path).await;

    assert_eq!(code, 0, "{document}");
    assert_eq!(calls.load(Ordering::SeqCst), 0, "it still never probes");
    let doc: serde_json::Value = serde_json::from_str(&document).expect("one JSON document");
    let got: Vec<(i64, &str)> = doc["accounts"]
        .as_array()
        .expect("accounts")
        .iter()
        .map(|a| {
            (
                a["number"].as_i64().expect("number"),
                a["email"].as_str().expect("email"),
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![(1, "alice@example.com"), (2, "bob@example.com")],
        "{document}"
    );
}

/// Slots follow the FILE, whatever order the server returns rows in: a row
/// the server lacks keeps the file's place, and one the file lost goes last.
#[tokio::test]
async fn overlay_live_rows_keeps_the_file_order() {
    let config = load_from(TWO_ACCOUNTS);
    let snapshot = snapshot_offline(
        config,
        Arc::new(NoRefresh),
        Arc::new(FixedProber { util: 0.25 }),
        false,
    )
    .await;
    let colors = std::collections::BTreeMap::new();
    let file_rows = account_status_rows(
        &snapshot,
        &[],
        StatusSource::Offline,
        None,
        false,
        None,
        &colors,
    );
    // The server knows bob (first, as if the file was reordered after it
    // booted) and carol (since removed from the file), not alice.
    let mut bob = file_rows[1].clone();
    bob.source = "live".into();
    let mut carol = file_rows[0].clone();
    carol.name = "carol@example.com".into();
    carol.source = "live".into();

    let rows = overlay_live_rows(file_rows, vec![bob, carol]);
    let got: Vec<(&str, &str)> = rows
        .iter()
        .map(|r| (r.name.as_str(), r.source.as_str()))
        .collect();
    assert_eq!(
        got,
        vec![
            ("alice@example.com", "offline"),
            ("bob@example.com", "live"),
            ("carol@example.com", "live"),
        ]
    );
}
