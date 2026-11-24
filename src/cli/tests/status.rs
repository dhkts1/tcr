//! Tests for `tcr status` in `cli/status.rs`: the `--check` rule, version skew, and the
//! status text with its verdicts and hints.

use super::*;
/// The text line's provenance tail. With a server it names both builds; with
/// none it still names the CLI's own, so "which binary am I running" is
/// always answerable from the first line of output.
#[test]
fn build_fields_name_the_server_and_the_client() {
    let server = BuildInfo {
        sha: "cd146ce".to_string(),
        dirty: Some(false),
        built_at: "2026-07-26T00:00:00Z".to_string(),
    };
    let live = build_fields(Some(&server), &BuildInfo::current());
    assert!(live.contains("server_sha=cd146ce"), "{live}");
    assert!(live.contains("server_dirty=false"), "{live}");
    assert!(
        live.contains("server_built_at=2026-07-26T00:00:00Z"),
        "{live}"
    );
    assert!(
        live.contains(&format!("client_sha={}", build_info::SHA)),
        "{live}"
    );

    let offline = build_fields(None, &BuildInfo::current());
    assert!(
        !offline.contains("server_sha="),
        "no server, no server fields: {offline}"
    );
    assert!(
        offline.contains(&format!("client_sha={}", build_info::SHA)),
        "{offline}"
    );

    // An unknown dirty flag reads as `unknown`, never as `false`.
    let murky = build_fields(Some(&BuildInfo::default()), &BuildInfo::current());
    assert!(murky.contains("server_dirty=unknown"), "{murky}");
}

/// With no live server there is nothing whose code could be stale, so the
/// skew check stays silent regardless of what the checkout looks like.
#[test]
fn skew_report_is_silent_without_a_server() {
    assert_eq!(skew_report(None), None);
}

// ---------------------------------------------------------------------
// `tcr sessions --json`
// ---------------------------------------------------------------------

/// The `--check` rule, one probe outcome at a time, on a row produced by
/// the real offline snapshot (a 401 from the scripted prober reads `error`)
/// and then re-labelled per case. The positive control comes first: the
/// unmodified row must fail the check, or every `Clean` below is vacuous.
#[tokio::test]
async fn status_check_fails_only_on_an_enabled_account_whose_probe_failed() {
    use crate::probe::ProbeStatus;
    let path = write_config("status-check", EXPIRED_ACCOUNT);
    let snapshot = snapshot_offline(
        load(&path),
        Arc::new(NoRefresh),
        Arc::new(RejectingProber),
        true,
    )
    .await;
    fs::remove_file(&path).ok();
    assert_eq!(snapshot.accounts[0].probe_status, ProbeStatus::Error);
    assert_eq!(StatusCheck::of(&snapshot), StatusCheck::ProbeFailed);
    assert_eq!(StatusCheck::ProbeFailed.exit_code(), 4);
    assert_eq!(StatusCheck::Clean.exit_code(), 0);

    let relabel = |probe: ProbeStatus, disabled: bool| {
        let mut s = snapshot.clone();
        s.accounts[0].probe_status = probe;
        s.accounts[0].disabled = disabled;
        StatusCheck::of(&s)
    };
    for failed in [
        ProbeStatus::Error,
        ProbeStatus::Timeout,
        ProbeStatus::UpstreamDown,
    ] {
        assert_eq!(
            relabel(failed, false),
            StatusCheck::ProbeFailed,
            "{failed:?}"
        );
        assert_eq!(
            relabel(failed, true),
            StatusCheck::Clean,
            "a disabled account never fails the check ({failed:?})"
        );
    }
    for benign in [
        ProbeStatus::Never,
        ProbeStatus::Ok,
        ProbeStatus::RateLimited,
    ] {
        assert_eq!(relabel(benign, false), StatusCheck::Clean, "{benign:?}");
    }
}

/// **`tcr status --check` counts a Codex row's failed probe like any other.** The
/// rule is "an enabled account's last probe failed", and a Codex account is probed
/// at its own provider's usage endpoint, so its failure is a probe failure of the
/// fleet: exit 4. The same row disabled is out of the rule, as an Anthropic one is.
/// Built through the real offline snapshot over a prober that refuses only Codex
/// credentials, so the row that fails is failed by its OWN provider's probe, and the
/// Anthropic row's healthy probe is the control.
///
/// Watched red: exempt a Codex row in `probe_failed`, and the enabled case reads
/// clean.
#[tokio::test]
async fn status_check_counts_an_enabled_codex_rows_failed_probe() {
    struct CodexRefusingProber {
        seen: Arc<std::sync::Mutex<Vec<Provider>>>,
    }
    impl UsageProber for CodexRefusingProber {
        fn probe(&self, credential: crate::probe::ProbeCredential) -> ProbeFuture {
            let provider = credential.provider;
            self.seen.lock().expect("seen lock poisoned").push(provider);
            Box::pin(async move {
                match provider {
                    Provider::Anthropic => Ok(Usage {
                        five_hour: crate::probe::WindowReading::Read(UsageBucket {
                            utilization: Some(0.2),
                            reset_at_ms: None,
                        }),
                        seven_day: None,
                        seven_day_oi: None,
                        extra_usage_usd: None,
                        codex: None,
                        claude_resets: None,
                    }),
                    Provider::Codex => Err(crate::probe::ProbeError {
                        status: Some(401),
                        message: "HTTP 401: token rejected".to_string(),
                        retry_after_secs: None,
                    }),
                }
            })
        }
    }

    for (disabled, want) in [
        (false, StatusCheck::ProbeFailed),
        (true, StatusCheck::Clean),
    ] {
        let config: Config = serde_json::from_str(&format!(
                r#"{{ "accounts": [
                     {{ "name": "alice@example.com", "accessToken": "at-a", "refreshToken": "rt-a",
                        "expiresAt": 4102444800000 }},
                     {{ "name": "bob@example.com/codex", "provider": "codex", "accessToken": "at-b",
                        "refreshToken": "rt-b", "expiresAt": 4102444800000,
                        "accountUuid": "22222222-2222-4222-8222-222222222222", "disabled": {disabled} }}
                   ] }}"#
            ))
            .expect("a test config parses");
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let snapshot = snapshot_offline(
            config,
            Arc::new(NoRefresh),
            Arc::new(CodexRefusingProber {
                seen: Arc::clone(&seen),
            }),
            true,
        )
        .await;
        assert_eq!(
            snapshot.accounts[0].probe_status,
            ProbeStatus::Ok,
            "control: the Anthropic row's own probe is healthy"
        );
        assert_eq!(StatusCheck::of(&snapshot), want, "disabled: {disabled}");
        if !disabled {
            assert_eq!(snapshot.accounts[1].provider, Provider::Codex);
            assert_eq!(snapshot.accounts[1].probe_status, ProbeStatus::Error);
            assert!(
                seen.lock()
                    .expect("seen lock poisoned")
                    .contains(&Provider::Codex),
                "the Codex row was probed with a Codex credential"
            );
        }
    }
}

// --- no-refresh guarantee (Fix 1) --------------------------------------

/// `tcr status`'s text form over the contract sample's Anthropic rows, as a live
/// server at `abc1234` answers a client at `def5678`, with `claude_verdict` above the
/// header when there is one.
fn claude_only_status_text(claude_verdict: Option<&str>) -> String {
    let (mut snapshot, _) = status_contract_snapshot();
    snapshot
        .accounts
        .retain(|row| row.provider == Provider::Anthropic);
    let server = BuildInfo {
        sha: "abc1234".to_string(),
        dirty: Some(false),
        built_at: "2026-01-01T00:00:00Z".to_string(),
    };
    let client = BuildInfo {
        sha: "def5678".to_string(),
        dirty: Some(false),
        built_at: "2026-01-01T00:00:00Z".to_string(),
    };
    // The group colors `status_contract_rendered` builds.
    let group_colors: std::collections::BTreeMap<String, String> = [
        (
            "codereview".to_string(),
            config::derive_group_color("codereview"),
        ),
        ("dev".to_string(), "#0a84ff".to_string()),
    ]
    .into_iter()
    .collect();
    render_status_text(&StatusText {
        snapshot: &snapshot,
        source: StatusSource::Live,
        server_build: Some(&server),
        client_build: &client,
        http1_only: false,
        group_colors: &group_colors,
        claude_verdict: claude_verdict.map(str::to_string),
        // Not pooled, and a Claude-only fleet still prints no Codex line.
        codex_route: Some(&mitm_off_route()),
        holds_codex: holds_provider(&snapshot, Provider::Codex),
    })
}

/// The bytes `tcr status` prints for a fleet with no Codex account, pinned before the
/// status text learned about Codex accounts: every line a Codex fleet gains must leave a
/// Claude-only fleet's text byte for byte as it was, the route verdict above the header
/// included.
#[test]
fn claude_only_status_text_is_unchanged() {
    /// The pinned bytes, below any verdict line.
    const FLEET: &str = "status source=live server_sha=abc1234 server_dirty=false \
             server_built_at=2026-01-01T00:00:00Z client_sha=def5678 http1Only=false \
             groupColors=codereview:#63cbfe,dev:#0a84ff\n\
             account alice@example.com priority=0 plan=Max 20x 5h=4% wk=1% fable=0% cache=75% \
             state=ok status=active probe=ok stream_errors=2 last_stream_error=overloaded_error \
             groups=codereview,dev\n\
             account bob@example.com priority=1 plan=Team 5x 5h=95% wk=91% fable=10% cache=75% \
             state=near status=active probe=rate-limited stream_errors=2 \
             last_stream_error=overloaded_error\n\
             account carol@example.com priority=2 plan=Team Standard 5h=100% wk=100% fable=50% \
             cache=75% state=spent status=throttled probe=error stream_errors=2 \
             last_stream_error=overloaded_error\n\
             account dave@example.com priority=3 5h=n/a wk=n/a cache=n/a state=ok status=active \
             probe=never stream_errors=0 disabled\n";
    assert_eq!(claude_only_status_text(None), FLEET);
    assert_eq!(
        claude_only_status_text(Some(
            "verdict: this proxy is not on Claude's route: \
                 /Users/example/.claude/settings.json sets https://gateway.example.com"
        )),
        format!(
            "verdict: this proxy is not on Claude's route: \
                 /Users/example/.claude/settings.json sets https://gateway.example.com\n{FLEET}"
        )
    );
}

/// The Claude verdict the text-form tests print above the header.
const CLAUDE_VERDICT: &str = "verdict: this proxy is not on Claude's route: \
                                  /Users/example/.claude/settings.json sets \
                                  https://gateway.example.com";

/// The MITM-off route's reason, as `tcr run --codex` words it for a proxy on :3999.
const MITM_OFF_REASON: &str = "the proxy on :3999 is running without MITM: its TLS material \
                                   failed to load at boot — see its log";

/// The status read of a proxy on :3999, as `fetch_live_status` hands it over: parsed from
/// the wire, with `mitm` as given, `codex_rows` Codex accounts and `traffic` as its counts.
fn live_payload(
    mitm: serde_json::Value,
    codex_rows: usize,
    traffic: Option<serde_json::Value>,
) -> StatusPayload {
    let mut wire = crate::test_support::status_wire::codex_status_wire(Some(mitm), codex_rows);
    if let Some(traffic) = traffic {
        wire["codex_traffic"] = traffic;
    }
    serde_json::from_value(wire).expect("a status payload")
}

fn route_test_ca() -> &'static Path {
    Path::new("/tmp/tcr-run-test/tcr-ca.pem")
}

/// The route of a proxy on :3999 running without MITM: not pooled.
fn mitm_off_route() -> tcr_status_wire::CodexRoute {
    let payload = live_payload(
        serde_json::json!({"interceptedHosts": [], "leaf": "none"}),
        1,
        None,
    );
    crate::codex_route::codex_route(Ok(&payload), 3999, route_test_ca(), true, 1)
}

fn pooled_route() -> tcr_status_wire::CodexRoute {
    tcr_status_wire::CodexRoute {
        pooled: true,
        ..Default::default()
    }
}

#[test]
fn an_older_proxy_uses_configured_codex_provider_presence() {
    let old = live_payload(serde_json::Value::Null, 0, None);
    let config: Config = serde_json::from_value(serde_json::json!({
            "accounts": [{"name":"alice@example.com/codex", "provider":"codex", "accessToken":"at-fake"}]
        })).unwrap();
    assert!(status_holds_provider(&Ok(old), &config, Provider::Codex));
    let current = live_payload(
        serde_json::json!({"interceptedHosts": [], "leaf":"minted"}),
        0,
        None,
    );
    assert!(
        !status_holds_provider(&Ok(current), &config, Provider::Codex),
        "a current server's live deletion overrides stale configured rows"
    );
}

/// `tcr status`'s text form over `snapshot`, as the golden renders it, with the Claude
/// verdict and the Codex route given.
fn status_text_of(
    snapshot: &StatsSnapshot,
    claude_verdict: Option<&str>,
    codex_route: &tcr_status_wire::CodexRoute,
) -> String {
    let build = BuildInfo {
        sha: "abc1234".to_string(),
        dirty: Some(false),
        built_at: "2026-01-01T00:00:00Z".to_string(),
    };
    render_status_text(&StatusText {
        snapshot,
        source: StatusSource::Live,
        server_build: Some(&build),
        client_build: &build,
        http1_only: false,
        group_colors: &std::collections::BTreeMap::new(),
        claude_verdict: claude_verdict.map(str::to_string),
        codex_route: Some(codex_route),
        holds_codex: holds_provider(snapshot, Provider::Codex),
    })
}

/// **A fleet that holds a Codex account is told, above the header, when codex is not
/// pooled**, keyed `codexVerdict:` beside Claude's own verdict, which keeps the first line.
/// A pooled route prints nothing.
#[test]
fn the_codex_verdict_prints_only_with_a_codex_account_not_pooled() {
    // The contract sample: four Anthropic rows and a disabled Codex row.
    let (snapshot, _) = status_contract_snapshot();
    let off = mitm_off_route();
    let text = status_text_of(&snapshot, None, &off);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines[0],
        format!("codexVerdict: codex is not pooled: {MITM_OFF_REASON}")
    );
    assert!(lines[1].starts_with("status source="), "{text}");

    let text = status_text_of(&snapshot, Some(CLAUDE_VERDICT), &off);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], CLAUDE_VERDICT, "Claude's verdict stays first");
    assert_eq!(
        lines[1],
        format!("codexVerdict: codex is not pooled: {MITM_OFF_REASON}")
    );
    assert!(lines[2].starts_with("status source="), "{text}");

    let text = status_text_of(&snapshot, None, &pooled_route());
    assert!(text.starts_with("status source="), "{text}");
    assert!(!text.contains("codex is not pooled"), "{text}");
}

/// **A Codex-only fleet is not told about Claude's route**, which none of its accounts
/// uses: its one verdict is codex's, keyed `verdict:`. A pooled route leaves the header
/// first, and an empty fleet keeps Claude's verdict as it always had it.
///
/// Watched red: with Claude's verdict printed whatever the fleet holds, the Codex-only
/// fleet opens with Claude's route.
#[test]
fn the_claude_verdict_is_skipped_in_a_codex_only_fleet() {
    let (mut snapshot, _) = status_contract_snapshot();
    snapshot
        .accounts
        .retain(|row| row.provider == Provider::Codex);
    let off = mitm_off_route();
    let text = status_text_of(&snapshot, Some(CLAUDE_VERDICT), &off);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines[0],
        format!("verdict: codex is not pooled: {MITM_OFF_REASON}")
    );
    assert!(lines[1].starts_with("status source="), "{text}");
    assert!(
        !text.contains("this proxy is not on Claude's route"),
        "{text}"
    );

    let text = status_text_of(&snapshot, Some(CLAUDE_VERDICT), &pooled_route());
    assert!(text.starts_with("status source="), "{text}");

    snapshot.accounts.clear();
    let text = status_text_of(&snapshot, Some(CLAUDE_VERDICT), &off);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], CLAUDE_VERDICT, "an empty fleet keeps it");
    assert!(lines[1].starts_with("status source="), "{text}");
    assert!(!text.contains("codex is not pooled"), "{text}");
}

/// **The stderr notes say what tcr did with codex in the last ten minutes**, each only
/// when its count is above zero.
#[test]
fn status_prints_the_codex_traffic_notes_on_stderr() {
    let traffic = tcr_status_wire::CodexTraffic {
        refused_10m: 12,
        last_refusal: Some(tcr_status_wire::CodexRefusalKind::UsageLimitReached),
        last_refusal_at_ms: Some(1),
        unpooled_10m: 3,
    };
    assert_eq!(
        codex_traffic_notes(&traffic),
        [
            "[tcr] note codex-refusals: refused_10m=12 last_type=usage_limit_reached — tcr \
                 refused codex because no Codex account could serve; see the Codex \
                 rows above",
            "[tcr] note codex-unpooled: relayed_10m=3 — requests from a FedRAMP or \
                 residency-routed ChatGPT login ran on their own credential, unpooled",
        ]
    );
    assert!(
        codex_traffic_notes(&tcr_status_wire::CodexTraffic::default()).is_empty(),
        "nothing to say"
    );
    let refused_only = tcr_status_wire::CodexTraffic {
        unpooled_10m: 0,
        ..traffic
    };
    let notes = codex_traffic_notes(&refused_only);
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(
        notes[0].starts_with("[tcr] note codex-refusals:"),
        "{notes:?}"
    );
}

/// **`--json` carries a Codex route only from a live read.** Nothing listening, no answer,
/// or one this build cannot use: none of them is a route, so no row claims one.
#[test]
fn the_route_is_absent_offline() {
    for live in [
        Err(LiveStatusError::NoServer),
        Err(LiveStatusError::NoAnswer(
            "the server did not answer within 5s".to_string(),
        )),
        Err(LiveStatusError::Unusable(
            "HTTP 401 Unauthorized".to_string(),
        )),
    ] {
        let facts = codex_wire_facts(&live, 3999, route_test_ca(), true);
        assert_eq!(facts, (None, None), "{live:?}");
    }
}

/// **The route and the traffic ride the Codex rows only**: the same on every Codex row, and
/// on no Anthropic row, whose bytes stay what they were.
#[test]
fn route_and_traffic_ride_only_codex_rows() {
    let live = Ok(live_payload(
        serde_json::json!({
            "interceptedHosts": ["api.anthropic.com", "platform.claude.com", "chatgpt.com"],
            "leaf": "minted",
        }),
        1,
        Some(serde_json::json!({
            "refused10m": 2,
            "lastRefusal": "pool_unavailable",
            "lastRefusalAtMs": 1_767_225_000_000_i64,
            "unpooled10m": 0,
        })),
    ));
    let (route, traffic) = codex_wire_facts(&live, 3999, route_test_ca(), true);
    assert_eq!(route, Some(pooled_route()));
    assert_eq!(traffic.as_ref().map(|t| t.refused_10m), Some(2));

    let (snapshot, thresholds) = status_contract_snapshot();
    let build = BuildInfo {
        sha: "abc1234".to_string(),
        dirty: Some(false),
        built_at: "2026-01-01T00:00:00Z".to_string(),
    };
    let rendered = status_rows_json(account_status_rows_at(
        OffsetDateTime::from_unix_timestamp(1_767_225_600).expect("a valid instant"),
        &snapshot,
        &thresholds,
        StatusSource::Live,
        Some(&build),
        false,
        None,
        &std::collections::BTreeMap::new(),
        route.as_ref(),
        traffic.as_ref(),
    ));
    let rows: Vec<serde_json::Value> = serde_json::from_str(&rendered).expect("an array");
    assert_eq!(rows.len(), 5);
    let mut anthropic = 0;
    for row in &rows {
        if row["name"] == "bob@example.com/codex" {
            assert_eq!(
                row["codexRoute"],
                serde_json::to_value(&route).expect("a route")
            );
            assert_eq!(
                row["codexTraffic"],
                serde_json::to_value(&traffic).expect("the traffic")
            );
        } else {
            anthropic += 1;
            assert!(row.get("codexRoute").is_none(), "{row}");
            assert!(row.get("codexTraffic").is_none(), "{row}");
        }
    }
    assert_eq!(anthropic, 4);
}

/// A Claude row's banked resets on `tcr status`: per kind with the earliest use-by,
/// `unknown` for an ineligible answer (never `0`), `0` for eligible with none held, and
/// nothing when never read. The JSON carries the grants and never a grant id.
#[test]
fn claude_banked_resets_reach_text_and_json() {
    use tcr_status_wire::{BankedReset, BankedResets, ResetKind};
    let grant = |kind, left, ends| BankedReset {
        kind,
        resets_left: left,
        usable_now: true,
        paused: false,
        requires_limit: false,
        ends_at_ms: ends,
        unavailable_reason: None,
    };
    let held = BankedResets {
        eligible: true,
        reason: None,
        grants: vec![
            grant(ResetKind::Fable, 2, Some(1_891_036_800_000)),
            grant(ResetKind::Weekly, 1, Some(1_891_123_200_000)),
            grant(ResetKind::Weekly, 0, Some(1_800_000_000_000)),
        ],
    };
    assert_eq!(
        claude_resets_token(Some(&held)),
        " resets=weekly:1,fable:2(Dec 4)"
    );
    let surface = BankedResets {
        eligible: false,
        reason: Some("surface".to_owned()),
        grants: Vec::new(),
    };
    assert_eq!(claude_resets_token(Some(&surface)), " resets=unknown");
    let none = BankedResets {
        eligible: true,
        ..surface
    };
    assert_eq!(claude_resets_token(Some(&none)), " resets=0");
    assert_eq!(claude_resets_token(None), "");

    let (mut snapshot, thresholds) = status_contract_snapshot();
    assert_eq!(snapshot.accounts[0].provider, Provider::Anthropic);
    snapshot.accounts[0].codex.banked_resets = Some(held);
    let text = status_text_of(&snapshot, None, &pooled_route());
    assert!(text.contains(" resets=weekly:1,fable:2(Dec 4)"), "{text}");
    let wire: serde_json::Value =
        serde_json::from_str(&status_json(&snapshot, &thresholds, StatusSource::Live)).unwrap();
    assert_eq!(wire[0]["bankedResets"]["eligible"], true);
    assert_eq!(wire[0]["bankedResets"]["grants"][0]["kind"], "fable");
    assert_eq!(wire[0]["bankedResets"]["grants"][0]["resetsLeft"], 2);
    assert!(wire[4].get("bankedResets").is_none());
}

#[test]
fn reset_credits_reach_text_and_json_without_identities() {
    let (mut snapshot, thresholds) = status_contract_snapshot();
    let text = status_text_of(&snapshot, None, &pooled_route());
    assert!(text.contains(" resets=3 reset_expires_at_ms=1800000000000"));
    let wire: serde_json::Value =
        serde_json::from_str(&status_json(&snapshot, &thresholds, StatusSource::Live)).unwrap();
    assert_eq!(
        wire[4]["resetCredits"],
        serde_json::json!({
            "availableCount": 3, "earliestExpiryMs": 1_800_000_000_000i64
        })
    );
    assert!(wire[0].get("resetCredits").is_none());
    snapshot.accounts[4].codex.reset_credits = Some(tcr_status_wire::ResetCredits {
        available_count: 0,
        earliest_expiry_ms: None,
    });
    assert!(codex_tail(&snapshot.accounts[4]).contains(" resets=0"));
    assert!(!codex_tail(&snapshot.accounts[4]).contains("reset_expires_at_ms"));
    snapshot.accounts[4].codex.reset_credits = None;
    assert!(!codex_tail(&snapshot.accounts[4]).contains("resets="));
    snapshot.accounts[0].codex.reset_credits = Some(tcr_status_wire::ResetCredits {
        available_count: 9,
        earliest_expiry_ms: None,
    });
    assert!(!codex_tail(&snapshot.accounts[0]).contains("resets="));
    let wire: serde_json::Value =
        serde_json::from_str(&status_json(&snapshot, &thresholds, StatusSource::Live)).unwrap();
    assert!(wire[0].get("resetCredits").is_none());
    assert!(wire[4].get("resetCredits").is_none());
}

#[test]
fn codex_pending_token_sync_has_a_text_status_hint() {
    let (mut snapshot, _) = status_contract_snapshot();
    let account = &mut snapshot.accounts[0];
    account.provider = Provider::Codex;
    account.codex.token_sync_pending = true;
    assert!(codex_tail(account).contains("token_sync=pending"));
    account.provider = Provider::Anthropic;
    assert!(codex_tail(account).is_empty());
}

/// A Claude row for the header and re-login tests: probed, in no group, its plan known,
/// and gated `gate`.
fn claude_row(name: &str, gate: crate::stats::GateReason) -> AccountSnapshot {
    AccountSnapshot {
        gate,
        ..contract_account(
            name,
            0,
            "active",
            false,
            None,
            None,
            None,
            None,
            None,
            None,
            ProbeStatus::Ok,
            None,
            QuotaState::Normal,
            &[],
            None,
            (Some("claude_max"), Some("default_claude_max_20x"), None),
            (None, None),
        )
    }
}

/// A Codex row for the header and re-login tests, gated `gate`.
fn codex_row(name: &str, gate: crate::stats::GateReason) -> AccountSnapshot {
    AccountSnapshot {
        gate,
        ..canonical_codex_row(name, 0, "pro", "55555555-5555-4555-8555-555555555555")
    }
}

/// A mixed fleet's header counts each pool's eligible accounts: gate `ok` and not
/// disabled, over every row of that provider. A fleet of one provider, or of none, keeps
/// the header it always had.
#[test]
fn the_eligible_header_prints_only_in_mixed_fleets() {
    use crate::stats::GateReason;

    let alice = claude_row("alice@example.com", GateReason::Ok);
    let frank = claude_row("frank@example.com", GateReason::FiveHour);
    let bob = codex_row("bob@example.com/codex", GateReason::Ok);
    let dana = AccountSnapshot {
        disabled: true,
        ..codex_row("dana@example.com", GateReason::Ok)
    };
    let erin = codex_row("erin@example.com/codex", GateReason::Login);
    let all = vec![
        alice.clone(),
        frank.clone(),
        bob.clone(),
        dana.clone(),
        erin.clone(),
    ];

    assert_eq!(
        eligible_token(&snapshot_of(all.clone())),
        " eligible=claude:1/2,codex:1/3"
    );
    assert_eq!(eligible_token(&snapshot_of(vec![alice.clone(), frank])), "");
    assert_eq!(eligible_token(&snapshot_of(vec![bob, dana, erin])), "");
    assert_eq!(eligible_token(&snapshot_of(Vec::new())), "");

    // A Codex row held by an upstream 403 is out of rotation until its hold ends.
    let pat = codex_row("pat@example.com/codex", GateReason::Refused);
    assert_eq!(
        eligible_token(&snapshot_of(vec![alice, pat])),
        " eligible=claude:1/1,codex:0/1"
    );

    let build = |sha: &str| BuildInfo {
        sha: sha.to_string(),
        dirty: Some(false),
        built_at: "2026-01-01T00:00:00Z".to_string(),
    };
    let group_colors = std::collections::BTreeMap::new();
    let pooled = tcr_status_wire::CodexRoute {
        pooled: true,
        code: None,
        reason: None,
        upstream: None,
    };
    let text = render_status_text(&StatusText {
        snapshot: &snapshot_of(all),
        source: StatusSource::Live,
        server_build: Some(&build("abc1234")),
        client_build: &build("def5678"),
        http1_only: false,
        group_colors: &group_colors,
        claude_verdict: None,
        codex_route: Some(&pooled),
        holds_codex: true,
    });
    let header = text
        .lines()
        .find(|line| line.starts_with("status source="))
        .unwrap_or_else(|| panic!("no header:\n{text}"));
    assert!(
        header.ends_with(" eligible=claude:1/2,codex:1/3"),
        "the count ends the header: {header}"
    );
}

/// Each Codex row in LOGIN gets one stderr line naming the command that signs it in again,
/// and why its login died when that is known, in the words the dashboard uses. A Claude row
/// never does, and neither does a Codex row whose gate is not LOGIN.
#[test]
fn status_prints_one_relogin_hint_per_signed_out_codex_row() {
    use crate::stats::GateReason;

    let mut erin = codex_row("erin@example.com/codex", GateReason::Login);
    erin.codex.login_error = Some("refresh_token_reused".to_string());
    erin.codex.codex_home = Some("/Users/example/.codex".to_string());
    let ivy = codex_row("ivy@example.com/codex", GateReason::Login);
    let frank = claude_row("frank@example.com", GateReason::Login);
    let mut bob = codex_row("bob@example.com/codex", GateReason::Ok);
    bob.codex.login_error = Some("refresh_token_reused".to_string());

    assert_eq!(
        relogin_hint_lines(
            &snapshot_of(vec![erin, ivy, frank, bob]),
            Some(Path::new("/Users/example"))
        ),
        [
            "[tcr] erin@example.com/codex needs re-login: the codex CLI in ~/.codex \
                 refreshed this shared login first — tcr login --codex --account \
                 erin@example.com/codex",
            "[tcr] ivy@example.com/codex needs re-login — tcr login --codex --account \
                 ivy@example.com/codex",
        ]
    );
}
