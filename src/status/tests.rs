use super::*;

fn snapshot_with_counters() -> StatsSnapshot {
    StatsSnapshot {
        accounts: vec![AccountSnapshot {
            slot: 0,
            name: "alice@example.com".to_string(),
            provider: crate::provider::Provider::Anthropic,
            organization_type: Some("claude_team".to_string()),
            rate_limit_tier: Some("default_raven".to_string()),
            seat_tier: Some("team_standard".to_string()),
            plan_display_name: None,
            org_uuid: Some("11111111-1111-1111-1111-111111111111".to_string()),
            org_name: Some("Example Org".to_string()),
            priority: 0,
            status: "active".to_string(),
            disabled: false,
            five_hour: Some(0.42),
            five_hour_reset: from_ms(crate::now_ms() + 3_600_000),
            five_hour_absent_on_plan: false,
            seven_day: None,
            seven_day_reset: None,
            seven_day_oi: Some(0.11),
            seven_day_oi_reset: from_ms(crate::now_ms() + 3_600_000),
            requests: 7,
            input_tokens: 1_000,
            output_tokens: 200,
            cache_read_tokens: 750,
            cache_creation_tokens: 50,
            last_used: from_ms(crate::now_ms()),
            rate_limited_until: None,
            probe_status: ProbeStatus::Ok,
            last_probe: from_ms(crate::now_ms()),
            quota_updated_at: None,
            probe_error: None,
            quota_state: QuotaState::Normal,
            gate: GateReason::Ok,
            free_at: None,
            stream_error_count: 0,
            last_stream_error: None,
            groups: vec!["codereview".to_string()],
            reserved_groups: vec!["codereview".to_string()],
            parked_groups: Vec::new(),
            control_allowed_groups: vec!["codereview".to_string()],
            usage: None,
            codex: Default::default(),
        }],
        current: Some(0),
        current_by_provider: crate::stats::CurrentByProvider::default(),
        recent: Vec::new(),
        sessions: Vec::new(),
        wire_sessions: vec![tcr_status_wire::SessionRow {
            session_id: "sess-1".to_string(),
            account: Some("alice@example.com".to_string()),
            model: Some("claude-fable-5".to_string()),
            first_seen_ms: crate::now_ms() - 60_000,
            last_seen_ms: crate::now_ms(),
            requests: 3,
            input_tokens: 400,
            output_tokens: 100,
            cache_read_tokens: 200,
            cache_creation_tokens: 30,
            tools: tcr_status_wire::SessionToolsRow {
                calls: 2,
                errors: 0,
                timeouts: 1,
                timeouts_by_class: std::iter::once(("git-net".to_string(), 1u64)).collect(),
                timed_out: vec![tcr_status_wire::SlowToolRow {
                    tool: "Bash".to_string(),
                    seconds: 600.0,
                    command_head: Some("git push origin main".to_string()),
                    command_class: Some("git-net".to_string()),
                    ended_ms: crate::now_ms(),
                }],
                running: vec![tcr_status_wire::RunningToolRow {
                    tool: "Bash".to_string(),
                    started_ms: crate::now_ms() - 5_000,
                    command_head: Some("ls -la".to_string()),
                    command_class: Some("other".to_string()),
                    timeout_seconds: None,
                }],
                subagents_running: 0,
                slowest: vec![tcr_status_wire::SlowToolRow {
                    tool: "Bash".to_string(),
                    seconds: 3.5,
                    command_head: Some("sleep 3".to_string()),
                    command_class: Some("wait".to_string()),
                    ended_ms: crate::now_ms(),
                }],
                over_one_minute: 0,
                by_tool: vec![tcr_status_wire::ToolBucketRow {
                    tool: "Bash".to_string(),
                    calls: 2,
                    errors: 0,
                    seconds_p50: 3.5,
                    over_one_minute: 0,
                }],
            },
            req_per_minute: vec![0; 29].into_iter().chain(std::iter::once(3)).collect(),
            cost_usd: 0.0125,
            cache_ttl_secs: Some(300),
            provider: None,
            unpriced_requests: 0,
            reasoning_tokens: 0,
        }],
        wire_sessions_summary: tcr_status_wire::SessionsSummary {
            calls: 2,
            over_one_minute: 0,
            timeouts: 1,
            timeouts_by_class: std::iter::once(("git-net".to_string(), 1u64)).collect(),
            by_tool: vec![tcr_status_wire::ToolBucketRow {
                tool: "Bash".to_string(),
                calls: 2,
                errors: 0,
                seconds_p50: 0.0,
                over_one_minute: 0,
            }],
            cost_usd: 0.0125,
        },
    }
}

#[test]
fn quota_v1_contains_only_anthropic_capacity() {
    let mut snapshot = snapshot_with_counters();
    let mut codex = snapshot.accounts[0].clone();
    codex.name = "bob@example.com/codex".into();
    codex.provider = crate::provider::Provider::Codex;
    snapshot.accounts.push(codex.clone());
    let payload = QuotaPayload::from_snapshot(&snapshot);
    assert_eq!(payload.kind, "tcr.quota.v1");
    assert_eq!(
        payload
            .accounts
            .iter()
            .map(|row| row.name.as_str())
            .collect::<Vec<_>>(),
        ["alice@example.com"],
        "the documented v1 quota shape reports Claude capacity only"
    );
    snapshot.accounts = vec![codex];
    assert!(QuotaPayload::from_snapshot(&snapshot).accounts.is_empty());
}

/// Every rendered field survives the status wire round trip. The live path
/// must render the same numbers as offline, differing only in its source label.
#[test]
fn payload_round_trips_every_rendered_field() {
    let snapshot = snapshot_with_counters();
    let wire = serde_json::to_string(&StatusPayload::from_snapshot(
        &snapshot,
        &[0.85],
        false,
        None,
        Default::default(),
    ))
    .expect("serialize");
    let back: StatusPayload = serde_json::from_str(&wire).expect("deserialize");
    assert_eq!(back.kind, STATUS_KIND);
    let (rebuilt, thresholds) = back.into_snapshot();
    assert_eq!(thresholds, vec![0.85], "the server's threshold rides along");

    let (before, after) = (&snapshot.accounts[0], &rebuilt.accounts[0]);
    assert_eq!(after.name, before.name);
    assert_eq!(after.priority, before.priority);
    assert_eq!(after.status, before.status);
    assert_eq!(after.five_hour, before.five_hour);
    assert_eq!(after.five_hour_reset, before.five_hour_reset);
    assert_eq!(after.seven_day_oi, before.seven_day_oi);
    assert_eq!(
        after.seven_day_oi_reset, before.seven_day_oi_reset,
        "sevenDayOiResetAtMs rides the wire intact"
    );
    assert_eq!(after.requests, before.requests);
    assert_eq!(after.input_tokens, before.input_tokens);
    assert_eq!(after.output_tokens, before.output_tokens);
    assert_eq!(after.cache_read_tokens, before.cache_read_tokens);
    assert_eq!(after.cache_creation_tokens, before.cache_creation_tokens);
    assert_eq!(after.last_used, before.last_used);
    assert_eq!(after.probe_status, before.probe_status);
    assert_eq!(after.quota_state, before.quota_state);
    assert_eq!(after.gate, before.gate);
    assert_eq!(after.groups, before.groups, "groups rides the wire intact");
    assert_eq!(
        rebuilt.wire_sessions, snapshot.wire_sessions,
        "the sessions array rides the wire intact"
    );
    assert_eq!(
        after.reserved_groups, before.reserved_groups,
        "reservedGroups rides the wire intact"
    );
}

/// A payload from an older server that predates `reservedGroups` still
/// deserializes, with the field defaulting to empty — same forward-compat
/// contract as `groups` and `stream_error_count`.
#[test]
fn payload_without_reserved_groups_field_still_deserializes() {
    let wire = serde_json::to_string(&StatusPayload::from_snapshot(
        &snapshot_with_counters(),
        &[0.85],
        false,
        None,
        Default::default(),
    ))
    .expect("serialize");
    let mut value: serde_json::Value = serde_json::from_str(&wire).expect("parse");
    for account in value["accounts"].as_array_mut().expect("accounts array") {
        account
            .as_object_mut()
            .expect("account object")
            .remove("reservedGroups");
    }
    let stripped = serde_json::to_string(&value).expect("re-serialize");
    let back: StatusPayload =
        serde_json::from_str(&stripped).expect("deserialize without reservedGroups field");
    assert_eq!(
        back.accounts[0].reserved_groups,
        Vec::<String>::new(),
        "missing reservedGroups field on the wire defaults to empty, not a decode error"
    );
}

/// A payload from an older server that predates `groups` still deserializes,
/// with the field defaulting to empty — the same forward-compat contract
/// `stream_error_count` already relies on.
#[test]
fn payload_without_groups_field_still_deserializes() {
    let wire = serde_json::to_string(&StatusPayload::from_snapshot(
        &snapshot_with_counters(),
        &[0.85],
        false,
        None,
        Default::default(),
    ))
    .expect("serialize");
    // Simulate an older server: strip every `"groups":[...]` occurrence from
    // the account object rather than hand-writing a whole payload, so this
    // test tracks the real field name if it ever changes.
    let mut value: serde_json::Value = serde_json::from_str(&wire).expect("parse");
    for account in value["accounts"].as_array_mut().expect("accounts array") {
        account
            .as_object_mut()
            .expect("account object")
            .remove("groups");
    }
    let stripped = serde_json::to_string(&value).expect("re-serialize");
    let back: StatusPayload =
        serde_json::from_str(&stripped).expect("deserialize without groups field");
    assert_eq!(
        back.accounts[0].groups,
        Vec::<String>::new(),
        "missing groups field on the wire defaults to empty, not a decode error"
    );
}

/// A payload from an older server that predates `sevenDayOiResetMs` still
/// deserializes, defaulting to `None` — same forward-compat contract as
/// `reservedGroups` and `groups`.
#[test]
fn payload_without_seven_day_oi_reset_field_still_deserializes() {
    let mut snapshot = snapshot_with_counters();
    snapshot.accounts[0].seven_day_oi_reset = from_ms(crate::now_ms() + 3_600_000);
    let wire = serde_json::to_string(&StatusPayload::from_snapshot(
        &snapshot,
        &[0.85],
        false,
        None,
        Default::default(),
    ))
    .expect("serialize");
    let mut value: serde_json::Value = serde_json::from_str(&wire).expect("parse");
    for account in value["accounts"].as_array_mut().expect("accounts array") {
        account
            .as_object_mut()
            .expect("account object")
            .remove("sevenDayOiResetMs");
    }
    let stripped = serde_json::to_string(&value).expect("re-serialize");
    let back: StatusPayload =
        serde_json::from_str(&stripped).expect("deserialize without sevenDayOiResetMs field");
    assert_eq!(
        back.accounts[0].seven_day_oi_reset_ms, None,
        "missing sevenDayOiResetMs field on the wire defaults to None, not a decode error"
    );
}

/// A payload from an older server that predates the `sessions` array (F1) still
/// deserializes, defaulting to empty — same forward-compat contract as `groupColors`.
#[test]
fn payload_without_sessions_field_still_deserializes() {
    let wire = serde_json::to_string(&StatusPayload::from_snapshot(
        &snapshot_with_counters(),
        &[0.85],
        false,
        None,
        Default::default(),
    ))
    .expect("serialize");
    let mut value: serde_json::Value = serde_json::from_str(&wire).expect("parse");
    value
        .as_object_mut()
        .expect("payload object")
        .remove("sessions");
    let stripped = serde_json::to_string(&value).expect("re-serialize");
    let back: StatusPayload =
        serde_json::from_str(&stripped).expect("deserialize without a sessions field");
    assert_eq!(
        back.sessions,
        Vec::new(),
        "missing sessions field on the wire defaults to empty, not a decode error"
    );
}

/// A payload from an older server that predates `subagentsRunning` (F4) still
/// deserializes, defaulting to zero — same forward-compat contract as `sessions` itself.
#[test]
fn payload_without_subagents_running_field_still_deserializes() {
    let wire = serde_json::to_string(&StatusPayload::from_snapshot(
        &snapshot_with_counters(),
        &[0.85],
        false,
        None,
        Default::default(),
    ))
    .expect("serialize");
    let mut value: serde_json::Value = serde_json::from_str(&wire).expect("parse");
    for session in value["sessions"].as_array_mut().expect("sessions array") {
        session["tools"]
            .as_object_mut()
            .expect("tools object")
            .remove("subagentsRunning");
    }
    let stripped = serde_json::to_string(&value).expect("re-serialize");
    let back: StatusPayload =
        serde_json::from_str(&stripped).expect("deserialize without subagentsRunning field");
    assert_eq!(
        back.sessions[0].tools.subagents_running, 0,
        "missing subagentsRunning field on the wire defaults to 0, not a decode error"
    );
}

/// The server's build stamp rides along and survives the wire.
#[test]
fn payload_carries_the_servers_build() {
    let wire = serde_json::to_string(&StatusPayload::from_snapshot(
        &snapshot_with_counters(),
        &[0.85],
        false,
        None,
        Default::default(),
    ))
    .expect("serialize");
    let back: StatusPayload = serde_json::from_str(&wire).expect("deserialize");
    assert_eq!(
        back.build,
        BuildInfo::current(),
        "the payload reports the SERVING binary's build"
    );
    assert!(
        wire.contains("\"builtAt\""),
        "the build object is camelCase on the wire like every other field: {wire}"
    );
}

/// BACK-COMPAT, direction 1 — a NEW client reading an OLD server, whose
/// payload predates the `build` field entirely.
///
/// It must parse (a hard error would drop the client to the offline snapshot
/// and its structurally-zero counters, the exact regression this endpoint
/// exists to prevent) and it must report `unknown` — never a default that
/// reads as a real sha, and never anything a comparison could call in-sync.
#[test]
fn payload_without_a_build_field_parses_as_unknown() {
    let legacy = r#"{"kind":"tcr.status.v1","accounts":[]}"#;
    let payload: StatusPayload =
        serde_json::from_str(legacy).expect("an older server's payload still parses");
    assert_eq!(payload.kind, STATUS_KIND);
    assert_eq!(payload.build, BuildInfo::default());
    assert_eq!(payload.build.sha, crate::build_info::UNKNOWN);

    // A partial build object degrades per-field rather than failing the parse.
    let partial = r#"{"kind":"tcr.status.v1","accounts":[],"build":{"sha":"cd146ce"}}"#;
    let payload: StatusPayload = serde_json::from_str(partial).expect("partial build parses");
    assert_eq!(payload.build.sha, "cd146ce");
    assert_eq!(payload.build.dirty, None);
    assert_eq!(payload.build.built_at, crate::build_info::UNKNOWN);
}

/// BACK-COMPAT, direction 2 — an OLD client reading a NEW server. The old
/// binary's `StatusPayload` has no `build` field, so what keeps it working is
/// serde's default of IGNORING unknown fields. This test stands in for that
/// old struct: it would fail the day someone adds `deny_unknown_fields`,
/// which is what would silently break every un-rebuilt client.
#[test]
fn status_payload_tolerates_an_unknown_field() {
    let future = r#"{"kind":"tcr.status.v1","accounts":[],"build":{"sha":"cd146ce","dirty":false,"builtAt":"2026-07-26T00:00:00Z"},"somethingAddedLater":{"n":1}}"#;
    let payload: StatusPayload =
        serde_json::from_str(future).expect("an unknown field is skipped, not fatal");
    assert_eq!(payload.kind, STATUS_KIND, "and the kind never had to move");
    assert_eq!(payload.build.sha, "cd146ce");
}

/// THE SKEW THAT ACTUALLY HAPPENS HERE: a NEW client reading an OLD
/// server's payload — the binary on disk is rebuilt on merge while the live
/// process keeps serving until someone restarts it.
///
/// An account row with no `usage` key at all must deserialize, and read as
/// `None` — "this server does not report usage", which is true. Anything
/// that makes the absence a hard deserialize error instead drops
/// `tcr status` to the offline snapshot's structural zeros and shows a
/// fabricated healthy fleet; `stream_error_count` did exactly that on
/// 2026-08-04.
///
/// What this test actually catches is `usage` ceasing to be an `Option`
/// (verified by mutation — making it a required field turns this red).
/// Removing the field's `#[serde(default)]` does NOT, because serde already
/// reads an absent `Option` as `None`; the field's own doc-comment records
/// why, and which of its attributes is therefore load-bearing.
#[test]
fn a_payload_without_usage_reads_as_not_measured() {
    let old_server = r#"{"kind":"tcr.status.v1","accounts":[{"name":"alice@example.com","priority":0,"status":"active","disabled":false,"fiveHour":null,"fiveHourResetMs":null,"sevenDay":null,"sevenDayResetMs":null,"sevenDayOi":null,"requests":7,"inputTokens":1000,"outputTokens":200,"cacheReadTokens":750,"cacheCreationTokens":50,"lastUsedMs":null,"rateLimitedUntilMs":null,"probeStatus":"ok","lastProbeMs":null,"probeError":null,"quotaState":"normal","gate":"ok","freeAtMs":null,"threshold":0.85,"lastStreamError":null}]}"#;
    let payload: StatusPayload = serde_json::from_str(old_server)
        .expect("a payload with no usage key must still deserialize");
    let account = payload
        .accounts
        .first()
        .expect("the payload carries one account");
    assert_eq!(
        account.usage, None,
        "absent usage is NOT MEASURED, never a fabricated zero"
    );
    // And every counter the old server DID report survives untouched.
    assert_eq!(account.requests, 7);
    assert_eq!(account.cache_creation_tokens, 50);

    // The reconstructed snapshot carries the same absence through, so the
    // renderer one layer out has the same fact to work with.
    let (snapshot, _) = payload.into_snapshot();
    assert_eq!(snapshot.accounts[0].usage, None);
}

/// An older server sends no `quotaUpdatedAtMs`. The row must still decode,
/// read as `None` ("this server does not report it"), and carry that absence
/// into the rebuilt snapshot, never a fabricated "just now".
#[test]
fn a_payload_without_quota_updated_at_still_decodes() {
    let old_server = r#"{"kind":"tcr.status.v1","accounts":[{"name":"alice@example.com","priority":0,"status":"active","disabled":false,"fiveHour":0.5,"fiveHourResetMs":null,"sevenDay":null,"sevenDayResetMs":null,"sevenDayOi":null,"requests":7,"inputTokens":1000,"outputTokens":200,"cacheReadTokens":750,"cacheCreationTokens":50,"lastUsedMs":null,"rateLimitedUntilMs":null,"probeStatus":"ok","lastProbeMs":1767225600000,"probeError":null,"quotaState":"normal","gate":"ok","freeAtMs":null,"threshold":0.85,"lastStreamError":null}]}"#;
    let payload: StatusPayload = serde_json::from_str(old_server)
        .expect("a payload with no quotaUpdatedAtMs key must still deserialize");
    let account = payload.accounts.first().expect("one account");
    assert_eq!(account.quota_updated_at_ms, None);
    assert_eq!(account.last_probe_ms, Some(1_767_225_600_000));
    let (snapshot, _) = payload.into_snapshot();
    assert_eq!(snapshot.accounts[0].quota_updated_at, None);
}

/// The other direction: a value on the wire survives the server to CLI
/// round trip unchanged.
#[test]
fn quota_updated_at_round_trips_through_the_payload() {
    let mut snapshot = snapshot_with_counters();
    snapshot.accounts[0].quota_updated_at = from_ms(1_767_225_600_000);
    let wire = serde_json::to_string(&StatusPayload::from_snapshot(
        &snapshot,
        &[0.85],
        false,
        None,
        Default::default(),
    ))
    .expect("serialize");
    assert!(
        wire.contains("\"quotaUpdatedAtMs\":1767225600000"),
        "camelCase key carries the instant in ms: {wire}"
    );
    let back: StatusPayload = serde_json::from_str(&wire).expect("deserialize");
    let (rebuilt, _) = back.into_snapshot();
    assert_eq!(
        rebuilt.accounts[0].quota_updated_at,
        from_ms(1_767_225_600_000)
    );
}

/// The other direction: a usage object on the wire survives the round trip
/// with every dimension and the cost intact, so nothing is quietly dropped
/// between the server and the renderer.
#[test]
fn a_usage_row_round_trips_through_the_payload() {
    let mut snapshot = snapshot_with_counters();
    let totals = tcr_status_wire::UsageTotals {
        requests: 3,
        input_tokens: 1_000,
        cache_creation_tokens: 2_000,
        cache_creation_1h_tokens: 500,
        cache_read_tokens: 9_000,
        output_tokens: 400,
        cost_usd: Some(0.0425),
        unpriced_requests: 1,
        reasoning_tokens: 0,
    };
    snapshot.accounts[0].usage = Some(tcr_status_wire::UsageRow {
        today: totals,
        window: Some(tcr_status_wire::UsageWindow {
            since: 1_767_207_600_000,
            totals,
        }),
        last_hour: totals,
        today_by_model: std::iter::once(("claude-opus-5".to_string(), totals)).collect(),
        seven_day_window: None,
    });
    let wire = serde_json::to_string(&StatusPayload::from_snapshot(
        &snapshot,
        &[0.85],
        false,
        None,
        Default::default(),
    ))
    .expect("payload serializes");
    let (back, _) = serde_json::from_str::<StatusPayload>(&wire)
        .expect("payload deserializes")
        .into_snapshot();
    assert_eq!(
        back.accounts[0].usage, snapshot.accounts[0].usage,
        "usage crosses the wire unchanged"
    );
    // `since` rides inside the window object via `flatten`; prove the flatten
    // actually round-trips rather than trusting the derive.
    assert_eq!(
        back.accounts[0]
            .usage
            .as_ref()
            .and_then(|u| u.window)
            .map(|w| w.since),
        Some(1_767_207_600_000)
    );
}

/// The MIRROR of the test above, for the direction that actually broke: a
/// NEW client reading an OLD server's payload.
///
/// This is the normal state of the system, not an edge case — the on-disk
/// binary is rebuilt on merge while the live process keeps serving the old
/// one until someone restarts it, so `tcr status` routinely runs newer code
/// than the server it queries. On 2026-08-04 a client at bd60839 queried a
/// server at 325df03, could not parse the reply for want of
/// `streamErrorCount`, fell back to the offline snapshot, and reported all
/// 13 accounts `active` while the log carried 52 rate-limit events and 8
/// accounts sat on hour-long holds. A REQUIRED added field is the hazard;
/// `#[serde(default)]` is the fix, and this test is what keeps it.
///
/// The row below is deliberately spelled the way the OLD server emits it —
/// no `streamErrorCount`, no `lastStreamError`, no `freeAtFloorMs`.
#[test]
fn status_payload_parses_an_older_servers_row() {
    let old = r#"{"kind":"tcr.status.v1","accounts":[{"name":"a@example.com","priority":0,
        "status":"throttled","disabled":false,"requests":7,"inputTokens":1,"outputTokens":2,
        "cacheReadTokens":0,"cacheCreationTokens":0,"probeStatus":"ok","quotaState":"normal",
        "gate":"hold","threshold":0.9}]}"#;
    let payload: StatusPayload = serde_json::from_str(old)
        .expect("a newer client MUST parse an older server's row, not fall back to offline");
    let row = &payload.accounts[0];
    assert_eq!(row.stream_error_count, 0, "absent reads as 'not reported'");
    assert_eq!(row.last_stream_error, None);
    // The fields the old server DID send must survive intact — the point is
    // graceful degradation, not a payload parsed into defaults wholesale.
    assert_eq!(row.status, "throttled");
    assert_eq!(row.requests, 7);
}

/// BACK-COMPAT for `control` specifically: a NEW client reading an OLD
/// server's payload, which predates the `controlAccount` feature entirely
/// and carries no such key. Must parse — a hard error here would drop the
/// client to the offline snapshot, the exact regression `#[serde(default)]`
/// on every added field exists to prevent — and `control` must read as
/// `None`, the honest "the server never reported one", not a fabricated
/// name.
#[test]
fn status_payload_without_control_parses() {
    let old = r#"{"kind":"tcr.status.v1","accounts":[]}"#;
    let payload: StatusPayload =
        serde_json::from_str(old).expect("an older server's payload still parses");
    assert_eq!(payload.control, None);
}

/// `control` round-trips over the wire like every other field, and
/// `skip_serializing_if` means an UNSET control account does not even
/// appear on the wire (never a `null` literal) — the same "clear removes
/// the key" contract [`crate::config::save_control_account`] uses on disk.
#[test]
fn control_round_trips_and_absent_serializes_no_key() {
    let with_control = StatusPayload::from_snapshot(
        &snapshot_with_counters(),
        &[0.85],
        false,
        Some("alice@example.com".to_string()),
        Default::default(),
    );
    let wire = serde_json::to_string(&with_control).expect("serialize");
    assert!(wire.contains("\"control\":\"alice@example.com\""), "{wire}");
    let back: StatusPayload = serde_json::from_str(&wire).expect("deserialize");
    assert_eq!(back.control, Some("alice@example.com".to_string()));

    let without_control = StatusPayload::from_snapshot(
        &snapshot_with_counters(),
        &[0.85],
        false,
        None,
        Default::default(),
    );
    let wire = serde_json::to_string(&without_control).expect("serialize");
    assert!(
        !wire.contains("\"control\""),
        "an absent control account must not serialize a null: {wire}"
    );
}

/// `provider` crosses this wire for Codex rows only. An Anthropic row carries
/// no key, so an Anthropic-only fleet's payload is exactly what it was before
/// the field existed (its doc's claim), and a Codex row comes back as
/// `Provider::Codex`.
#[test]
fn provider_crosses_for_a_codex_row_and_is_absent_on_an_anthropic_one() {
    let anthropic = StatusPayload::from_snapshot(
        &snapshot_with_counters(),
        &[0.85],
        false,
        None,
        Default::default(),
    );
    let wire = serde_json::to_string(&anthropic).expect("serialize");
    assert!(
        !wire.contains("\"provider\""),
        "an Anthropic row carries no provider key: {wire}"
    );

    let mut snapshot = snapshot_with_counters();
    snapshot.accounts[0].provider = crate::provider::Provider::Codex;
    let codex = StatusPayload::from_snapshot(&snapshot, &[0.85], false, None, Default::default());
    let wire = serde_json::to_string(&codex).expect("serialize");
    assert!(wire.contains("\"provider\":\"codex\""), "{wire}");
    let back: StatusPayload = serde_json::from_str(&wire).expect("deserialize");
    assert_eq!(back.accounts[0].provider, crate::provider::Provider::Codex);
}

/// `mitm` is tri-state on the wire. A payload `from_snapshot` builds carries
/// no key at all, because only the serving process knows what its listener
/// intercepts and it always fills the key in; a payload without the key, an
/// older server's, reads as `None`; and a reported status crosses in
/// camelCase and round-trips intact, MITM off included.
#[test]
fn mitm_is_absent_until_the_serving_process_reports_it() {
    let mut payload = StatusPayload::from_snapshot(
        &snapshot_with_counters(),
        &[0.85],
        false,
        None,
        Default::default(),
    );
    let wire = serde_json::to_string(&payload).expect("serialize");
    assert!(
        !wire.contains("\"mitm\""),
        "no key until it is reported: {wire}"
    );
    let old: StatusPayload =
        serde_json::from_str(r#"{"kind":"tcr.status.v1","accounts":[]}"#).expect("parse");
    assert_eq!(old.mitm, None, "an older server reports no interception");

    for reported in [
        MitmStatus {
            intercepted_hosts: vec!["api.anthropic.com".to_string(), "chatgpt.com".to_string()],
            leaf: MitmStatus::LEAF_MINTED.to_string(),
        },
        MitmStatus::of(None),
    ] {
        payload.mitm = Some(reported.clone());
        let wire = serde_json::to_value(&payload).expect("serialize");
        assert_eq!(
            wire["mitm"],
            serde_json::json!({
                "interceptedHosts": reported.intercepted_hosts,
                "leaf": reported.leaf,
            }),
            "{wire}"
        );
        let back: StatusPayload = serde_json::from_value(wire).expect("deserialize");
        assert_eq!(back.mitm, Some(reported));
    }
}

/// `heat` carries no key until the serving process reads one, an older
/// server's payload reads as `None`, and a reading crosses in camelCase
/// and comes back whole.
#[test]
fn heat_is_absent_until_the_serving_process_reads_it() {
    let mut payload = StatusPayload::from_snapshot(
        &snapshot_with_counters(),
        &[0.85],
        false,
        None,
        Default::default(),
    );
    let wire = serde_json::to_string(&payload).expect("serialize");
    assert!(
        !wire.contains("\"heat\""),
        "no key until it is read: {wire}"
    );
    let old: StatusPayload =
        serde_json::from_str(r#"{"kind":"tcr.status.v1","accounts":[]}"#).expect("parse");
    assert_eq!(old.heat, None, "an older server reports no heat");

    let reading = tcr_status_wire::HeatRow {
        cpu_celsius: Some(93.25),
        fans: vec![tcr_status_wire::FanRow {
            index: 0,
            actual_rpm: 4976.0,
            min_rpm: 1350.0,
            max_rpm: 5777.0,
            manual: true,
            target_rpm: None,
        }],
        ftst: None,
    };
    payload.heat = Some(HeatField::Reading(reading.clone()));
    let wire = serde_json::to_value(&payload).expect("serialize");
    assert_eq!(
        wire["heat"],
        serde_json::json!({
            "cpuCelsius": 93.25,
            "fans": [{"index": 0, "actualRpm": 4976.0, "minRpm": 1350.0,
                      "maxRpm": 5777.0, "manual": true}],
        }),
        "{wire}"
    );
    let back: StatusPayload = serde_json::from_value(wire).expect("deserialize");
    assert_eq!(back.heat, Some(HeatField::Reading(reading)));

    // A reading with no CPU figure and no fans is still a reading: the SMC
    // opened and nothing read. It crosses as an empty object, not as absent.
    payload.heat = Some(HeatField::Reading(tcr_status_wire::HeatRow::default()));
    let wire = serde_json::to_value(&payload).expect("serialize");
    assert_eq!(wire["heat"], serde_json::json!({"fans": []}));
}

/// A `heat` this build cannot read costs the heat and nothing else: the
/// payload still parses, every other field intact, and the parser's words
/// are kept for the caller to report. `null` is no reading, as absent is.
#[test]
fn an_unreadable_heat_does_not_fail_the_payload() {
    let body = r#"{"kind":"tcr.status.v1","accounts":[],"http1_only":true,
        "heat":{"cpuCelsius":"hot","fans":[]}}"#;
    let payload: StatusPayload = serde_json::from_str(body).expect("still a payload");
    assert!(payload.http1_only);
    match payload.heat {
        Some(HeatField::Unreadable(why)) => assert!(why.contains("invalid type"), "{why}"),
        other => panic!("expected an unreadable heat, got {other:?}"),
    }
    let payload: StatusPayload =
        serde_json::from_str(r#"{"kind":"tcr.status.v1","accounts":[],"heat":null}"#)
            .expect("parse");
    assert_eq!(payload.heat, None);
}

/// A `fanControl` from a server older than the command counter (`sinceMs`, no `command`)
/// is unreadable and says which field it lacks: no other field stands in for the count.
#[test]
fn fan_status_without_the_command_counter_is_unreadable() {
    let body = r#"{"kind":"tcr.status.v1","accounts":[],
        "fanControl":{"desired":{"mode":"auto"},"state":"off","reason":null,
        "hotGuard":false,"targets":[],"detail":null,"sinceMs":1760000000000}}"#;
    let payload: StatusPayload = serde_json::from_str(body).expect("still a payload");
    match payload.fan_control {
        Some(FanControlField::Unreadable(why)) => assert!(why.contains("`command`"), "{why}"),
        other => panic!("expected an unreadable fanControl, got {other:?}"),
    }
}

/// A `fanControl` with a state this build has no name for (a newer server) costs fan
/// control and nothing else, and an absent key is no controller, never a default.
#[test]
fn fan_status_unreadable_or_absent_does_not_fail_the_payload() {
    let body = r#"{"kind":"tcr.status.v1","accounts":[],"http1_only":true,
        "fanControl":{"desired":{"mode":"auto"},"state":"hibernating","reason":null,
        "hotGuard":false,"targets":[],"detail":null,"command":0}}"#;
    let payload: StatusPayload = serde_json::from_str(body).expect("still a payload");
    assert!(payload.http1_only);
    assert!(
        matches!(payload.fan_control, Some(FanControlField::Unreadable(_))),
        "{:?}",
        payload.fan_control
    );
    let payload: StatusPayload =
        serde_json::from_str(r#"{"kind":"tcr.status.v1","accounts":[]}"#).expect("parse");
    assert_eq!(payload.fan_control, None);
    assert!(
        !serde_json::to_string(&payload)
            .unwrap()
            .contains("fanControl")
    );
}

/// Every Codex fact a server knows, the facts its snapshot groups under `codex`,
/// crosses this wire to the CLI and comes back as the facts it set, beside a usage row
/// carrying the weekly window and reasoning tokens.
///
/// `from_snapshot` and `into_snapshot` name every field by hand, which is this module's
/// no-secret discipline, and a field one of them forgets is a fact the CLI never
/// renders. A payload from a server that predates every Codex key rebuilds as
/// `CodexFacts::default()`: nothing reported, never a fabricated fact.
#[test]
fn account_status_carries_every_codex_field_through_the_snapshot() {
    let facts = CodexFacts {
        quota_windows: Some(vec!["fiveHour".into(), "sevenDay".into()]),
        five_hour_window_minutes: Some(300),
        seven_day_window_minutes: Some(10_080),
        limit_reached_type: Some("rate_limit_reached".into()),
        spend_control_reached: true,
        credits: Some(tcr_status_wire::CodexCredits {
            has_credits: true,
            unlimited: false,
            balance: Some("12.50".into()),
        }),
        login_error: Some("refresh_token_reused".into()),
        codex_home: Some("/Users/example/.codex".into()),
        token_sync_pending: true,
        banked_resets: None,
        auto_reset: None,
        reset_credits: Some(tcr_status_wire::ResetCredits {
            available_count: 3,
            earliest_expiry_ms: Some(1_800_000_000_000),
        }),
    };
    let totals = tcr_status_wire::UsageTotals {
        requests: 3,
        input_tokens: 1_000,
        cache_read_tokens: 800,
        output_tokens: 40,
        reasoning_tokens: 1_200,
        cost_usd: None,
        unpriced_requests: 3,
        ..Default::default()
    };
    let usage = tcr_status_wire::UsageRow {
        today: totals,
        window: None,
        last_hour: totals,
        today_by_model: std::iter::once(("gpt-5.1-codex".to_string(), totals)).collect(),
        seven_day_window: Some(tcr_status_wire::UsageWindow {
            since: 1_766_966_400_000,
            totals,
        }),
    };
    let mut snapshot = snapshot_with_counters();
    snapshot.accounts[0].provider = crate::provider::Provider::Codex;
    snapshot.accounts[0].codex = facts.clone();
    snapshot.accounts[0].usage = Some(usage.clone());

    let wire = serde_json::to_value(StatusPayload::from_snapshot(
        &snapshot,
        &[0.85],
        false,
        None,
        Default::default(),
    ))
    .expect("serialize");
    let account = &wire["accounts"][0];
    assert_eq!(
        account.get("quotaWindows"),
        Some(&serde_json::json!(["fiveHour", "sevenDay"])),
        "{account}"
    );
    assert_eq!(
        account.get("fiveHourWindowMinutes"),
        Some(&serde_json::json!(300)),
        "{account}"
    );
    assert_eq!(
        account.get("spendControlReached"),
        Some(&serde_json::json!(true)),
        "{account}"
    );
    assert_eq!(
        account.get("credits"),
        Some(&serde_json::json!({"hasCredits": true, "unlimited": false, "balance": "12.50"})),
        "{account}"
    );
    assert_eq!(
        account.get("loginError"),
        Some(&serde_json::json!("refresh_token_reused")),
        "{account}"
    );
    assert_eq!(
        account.get("codexHome"),
        Some(&serde_json::json!("/Users/example/.codex")),
        "{account}"
    );

    let back: StatusPayload = serde_json::from_value(wire).expect("deserialize");
    let (rebuilt, _) = back.into_snapshot();
    assert_eq!(
        rebuilt.accounts[0].codex, facts,
        "every Codex fact crosses the wire and comes back"
    );
    assert_eq!(
        rebuilt.accounts[0].usage,
        Some(usage),
        "the weekly window and the reasoning tokens cross with the rest of usage"
    );

    // The same row as an older server sends it: no Codex key at all.
    let old_server = r#"{"kind":"tcr.status.v1","accounts":[{"name":"alice@example.com","priority":0,"status":"active","disabled":false,"fiveHour":null,"fiveHourResetMs":null,"sevenDay":null,"sevenDayResetMs":null,"sevenDayOi":null,"requests":7,"inputTokens":1000,"outputTokens":200,"cacheReadTokens":750,"cacheCreationTokens":50,"lastUsedMs":null,"rateLimitedUntilMs":null,"probeStatus":"ok","lastProbeMs":null,"probeError":null,"quotaState":"normal","gate":"ok","freeAtMs":null,"threshold":0.85,"lastStreamError":null}]}"#;
    let payload: StatusPayload = serde_json::from_str(old_server)
        .expect("a payload with no Codex key must still deserialize");
    let (snapshot, _) = payload.into_snapshot();
    assert_eq!(
        snapshot.accounts[0].codex,
        CodexFacts::default(),
        "an older server reports no Codex fact, and none is invented"
    );
}

/// A plan that reported no usage window crosses this wire as `"quotaWindows": []` and
/// comes back as `Some(vec![])`, never as an absent key.
///
/// The empty list and the absent key are two different facts. `[]` says the plan has no
/// window, so the panel draws none; no key says the windows are unknown, so it draws
/// both rows. A Codex account's facts cross this server-to-CLI hop before
/// `tcr status --json` renders them, so an empty list skipped here would reach the
/// panel as "unknown" however carefully the row type downstream keeps the two apart.
#[test]
fn an_empty_quota_windows_list_crosses_the_payload_as_a_fact() {
    let mut snapshot = snapshot_with_counters();
    snapshot.accounts[0].provider = crate::provider::Provider::Codex;
    snapshot.accounts[0].codex.quota_windows = Some(Vec::new());

    let wire = serde_json::to_value(StatusPayload::from_snapshot(
        &snapshot,
        &[0.85],
        false,
        None,
        Default::default(),
    ))
    .expect("serialize");
    assert_eq!(
        wire["accounts"][0].get("quotaWindows"),
        Some(&serde_json::json!([])),
        "[] is a fact: the plan reported no window"
    );

    let back: StatusPayload = serde_json::from_value(wire).expect("deserialize");
    let (rebuilt, _) = back.into_snapshot();
    assert_eq!(
        rebuilt.accounts[0].codex.quota_windows,
        Some(Vec::new()),
        "[] is a fact: the plan reported no window"
    );
}

/// The server's payload parses with a key this build does not know, at the top level
/// and inside every object that gained a Codex key: an account row, a session row, a
/// running tool and `codex_traffic`.
///
/// `status_payload_tolerates_an_unknown_field` covers the top level only. An older CLI
/// reads a newer server by skipping what it does not know, at every depth, and this is
/// what fails the day one of these types starts rejecting unknown keys. The payload is
/// literal text, written the way a newer server would send it.
#[test]
fn status_payload_parses_with_an_unknown_key() {
    let newer = r#"{"kind":"tcr.status.v1","addedLater":{"n":1},
        "accounts":[{"name":"a@example.com","priority":0,"status":"active","disabled":false,
            "requests":7,"inputTokens":1,"outputTokens":2,"cacheReadTokens":0,
            "cacheCreationTokens":0,"probeStatus":"ok","quotaState":"normal","gate":"ok",
            "threshold":0.85,"provider":"codex","quotaWindows":["sevenDay"],
            "addedLater":{"n":1}}],
        "sessions":[{"sessionId":"019a3c5e-0000-7000-8000-000000000002",
            "firstSeenMs":1767224880000,"lastSeenMs":1767225599000,"provider":"codex",
            "addedLater":{"n":1},
            "tools":{"running":[{"tool":"exec_command","startedMs":1767225555000,
                "timeoutSeconds":120,"addedLater":{"n":1}}]}}],
        "codex_traffic":{"refused10m":12,"unpooled10m":3,"addedLater":{"n":1}},
        "codex_upstream":"http://127.0.0.1:9"}"#;
    let payload: StatusPayload =
        serde_json::from_str(newer).expect("an unknown key is skipped at every depth");
    assert_eq!(
        payload.accounts[0].quota_windows,
        Some(vec!["sevenDay".to_string()])
    );
    assert_eq!(payload.sessions[0].provider.as_deref(), Some("codex"));
    assert_eq!(
        payload.sessions[0].tools.running[0].timeout_seconds,
        Some(120)
    );
    assert_eq!(
        payload.codex_traffic,
        Some(tcr_status_wire::CodexTraffic {
            refused_10m: 12,
            unpooled_10m: 3,
            ..Default::default()
        })
    );
    assert_eq!(
        payload.codex_upstream.as_deref(),
        Some("http://127.0.0.1:9")
    );
}

#[test]
fn status_payload_keeps_the_fleet_when_the_refusal_kind_is_newer() {
    let payload: StatusPayload = serde_json::from_value(serde_json::json!({
        "kind": "tcr.status.v1",
        "accounts": [],
        "codex_traffic": {
            "refused10m": 12,
            "unpooled10m": 3,
            "lastRefusal": "future_refusal",
        },
        "codex_upstream": "http://127.0.0.1:9",
    }))
    .expect("a newer refusal kind must not discard the serving process's whole status");
    let traffic = payload.codex_traffic.unwrap();
    assert_eq!(traffic.refused_10m, 12);
    assert_eq!(traffic.unpooled_10m, 3);
    assert_eq!(traffic.last_refusal, None);
    assert_eq!(
        payload.codex_upstream.as_deref(),
        Some("http://127.0.0.1:9")
    );
}

/// An Anthropic account's row on this wire carries none of the Codex keys, its usage
/// no weekly window and no reasoning tokens, and the payload neither fleet-wide Codex
/// key; a payload with neither key reads both as `None`.
///
/// The compatibility promise is that an Anthropic-only fleet's payload is the bytes it
/// was before Codex existed. Every new key is skipped when it holds nothing, and this
/// is what fails when one of them is written as `null` or `false` instead.
#[test]
fn an_anthropic_account_status_serializes_with_no_new_key() {
    let totals = tcr_status_wire::UsageTotals {
        requests: 3,
        input_tokens: 1_000,
        cache_creation_tokens: 2_000,
        cache_creation_1h_tokens: 500,
        cache_read_tokens: 9_000,
        output_tokens: 400,
        cost_usd: Some(0.0425),
        unpriced_requests: 1,
        reasoning_tokens: 0,
    };
    let mut snapshot = snapshot_with_counters();
    snapshot.accounts[0].usage = Some(tcr_status_wire::UsageRow {
        today: totals,
        window: Some(tcr_status_wire::UsageWindow {
            since: 1_767_207_600_000,
            totals,
        }),
        last_hour: totals,
        today_by_model: std::iter::once(("claude-opus-5".to_string(), totals)).collect(),
        seven_day_window: None,
    });
    assert_eq!(
        snapshot.accounts[0].provider,
        crate::provider::Provider::Anthropic
    );

    let wire = serde_json::to_value(StatusPayload::from_snapshot(
        &snapshot,
        &[0.85],
        false,
        None,
        Default::default(),
    ))
    .expect("serialize");
    let account = wire["accounts"][0]
        .as_object()
        .expect("an account is an object");
    for key in [
        "quotaWindows",
        "fiveHourWindowMinutes",
        "sevenDayWindowMinutes",
        "limitReachedType",
        "spendControlReached",
        "credits",
        "loginError",
        "codexHome",
    ] {
        assert!(
            !account.contains_key(key),
            "an Anthropic account carries no {key}: {account:?}"
        );
    }
    let usage = account["usage"].as_object().expect("usage is an object");
    assert!(
        !usage.contains_key("sevenDayWindow"),
        "no weekly window, no sevenDayWindow key: {usage:?}"
    );
    let mut buckets = vec![&usage["today"], &usage["lastHour"], &usage["window"]];
    buckets.extend(
        usage["todayByModel"]
            .as_object()
            .expect("todayByModel is an object")
            .values(),
    );
    for bucket in buckets {
        let bucket = bucket.as_object().expect("a bucket is an object");
        assert!(
            !bucket.contains_key("reasoningTokens"),
            "a bucket with no reasoning tokens carries no key: {bucket:?}"
        );
    }
    let payload = wire.as_object().expect("the payload is an object");
    assert!(
        !payload.contains_key("codex_traffic"),
        "no codex_traffic key until the serving process reports one"
    );
    assert!(
        !payload.contains_key("codex_upstream"),
        "no codex_upstream key until the serving process reports one"
    );

    let old: StatusPayload =
        serde_json::from_str(r#"{"kind":"tcr.status.v1","accounts":[]}"#).expect("parse");
    assert_eq!(
        old.codex_traffic, None,
        "an older server reports no traffic"
    );
    assert_eq!(
        old.codex_upstream, None,
        "an older server reports no upstream"
    );
}

/// The `kind` discriminator is what stops an older server's upstream-forwarded
/// Anthropic error from being read as a status payload. Assert the exact
/// literal, since the client compares against it.
#[test]
fn payload_carries_the_kind_discriminator() {
    let wire = serde_json::to_string(&StatusPayload::from_snapshot(
        &snapshot_with_counters(),
        &[0.9],
        false,
        None,
        Default::default(),
    ))
    .expect("serialize");
    assert!(
        wire.contains("\"kind\":\"tcr.status.v1\""),
        "payload names its kind: {wire}"
    );
}
