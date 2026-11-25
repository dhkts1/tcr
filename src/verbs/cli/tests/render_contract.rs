//! Tests that hold `tcr status --json` (`cli/render.rs`) to the committed contract fixture,
//! and the group, plan, Fable and parked fields of its rows.

use super::*;
/// THE CROSS-LANGUAGE CONTRACT PIN.
///
/// `tcr status --json` is decoded by TcrBar (`FleetStatus.swift`), and no
/// compiler checks that seam — the key names cross as strings. This test
/// asserts the renderer still produces the committed fixture byte-for-byte,
/// and `RealWorldDecodeTests.testCommittedContractFixtureDecodes` decodes
/// **that same file** through `Fleet.decode`.
///
/// So a renamed key cannot land quietly: this test goes red first, and
/// regenerating the fixture to satisfy it (`TCR_UPDATE_FIXTURES=1 cargo test
/// status_contract_fixture_matches_committed`) turns the Swift side red in
/// the same breath. Both reds were observed before this was believed.
#[test]
fn status_contract_fixture_matches_committed() {
    let path = status_contract_fixture_path();
    let rendered = status_contract_rendered();

    if std::env::var_os("TCR_UPDATE_FIXTURES").is_some() {
        fs::create_dir_all(path.parent().expect("fixture dir")).expect("create fixture dir");
        fs::write(&path, &rendered).expect("write fixture");
        eprintln!("regenerated {}", path.display());
        return;
    }

    let committed = fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "cannot read the committed contract fixture at {}: {e}. \
                 Regenerate with TCR_UPDATE_FIXTURES=1 cargo test \
                 status_contract_fixture_matches_committed",
            path.display()
        )
    });

    assert_eq!(
        committed,
        rendered,
        "`tcr status --json` no longer renders the committed contract \
             fixture at {}. If the change is intended, regenerate it with \
             TCR_UPDATE_FIXTURES=1 and then run the Swift decode suite \
             (apps/macos: swift test) — that is the half of the contract this \
             process cannot check.",
        path.display()
    );
}

/// The fixture is only worth pinning if it is *representative*: a sample
/// covering one shape would pin one shape. Asserted against the committed
/// bytes rather than the renderer, so a fixture regenerated from a
/// narrowed snapshot fails here too.
#[test]
fn status_contract_fixture_covers_every_rendered_shape() {
    let committed = fs::read_to_string(status_contract_fixture_path())
        .expect("committed contract fixture is readable");
    let rows: Vec<serde_json::Value> =
        serde_json::from_str(&committed).expect("fixture is a bare JSON array");
    assert_eq!(rows.len(), 5, "one row per covered shape");

    let states: Vec<&str> = rows
        .iter()
        .map(|r| r["quotaState"].as_str().expect("quotaState is a string"))
        .collect();
    assert!(states.contains(&"ok"), "{states:?}");
    assert!(states.contains(&"near"), "{states:?}");
    assert!(states.contains(&"spent"), "{states:?}");

    let probes: Vec<&str> = rows
        .iter()
        .map(|r| r["probeStatus"].as_str().expect("probeStatus is a string"))
        .collect();
    assert!(probes.contains(&"never"), "{probes:?}");
    assert!(probes.contains(&"ok"), "{probes:?}");
    assert!(probes.contains(&"rate-limited"), "{probes:?}");
    assert!(probes.contains(&"error"), "{probes:?}");

    // The never-probed row: every optional the Swift model types as
    // optional really is null somewhere in this sample. `get`, not
    // indexing — indexing a MISSING key also yields Null, which would let a
    // deleted key pass as a null.
    let never = rows
        .iter()
        .find(|r| r["probeStatus"] == "never")
        .expect("a never-probed row");
    for key in [
        "quota",
        "fiveHour",
        "sevenDay",
        "sevenDayOi",
        "cacheHitRatio",
    ] {
        assert_eq!(
            never.get(key),
            Some(&serde_json::Value::Null),
            "{key} is present AND null on the never-probed row: {never}"
        );
    }
    assert_eq!(never["disabled"], serde_json::json!(true));

    // At least one row carries a `held` window, one carries a probe error,
    // and one carries a stream error — the three nested/optional shapes the
    // panel renders differently.
    assert!(
        rows.iter()
            .any(|r| !r["held"].as_array().expect("held is an array").is_empty()),
        "some row is held: {committed}"
    );
    assert!(
        rows.iter().any(|r| r["probeError"].is_string()),
        "some row carries a probe error: {committed}"
    );
    assert!(
        rows.iter().any(|r| r["lastStreamError"].is_string()),
        "some row carries a stream error: {committed}"
    );
    assert!(
        rows.iter().any(|r| r["freeAtMs"].is_i64()),
        "some row pins freeAtMs as a number: {committed}"
    );
    assert!(
        rows.iter().any(|r| r["rateLimitedUntilMs"].is_i64()),
        "some row pins rateLimitedUntilMs as a number: {committed}"
    );
    assert!(
        rows.iter().any(|r| !r["groups"]
            .as_array()
            .expect("groups is an array")
            .is_empty()),
        "some row carries non-empty groups: {committed}"
    );
    assert!(
        rows.iter().any(|r| r["groups"]
            .as_array()
            .expect("groups is an array")
            .is_empty()),
        "some row carries no groups, rendered as [] not null: {committed}"
    );
    // `groupColors` is server-wide (like `http1Only`), so it is identical
    // on every row — checking row 0 checks all of them. Real entries, not
    // an empty object: the fixture must exercise both a configured color
    // ("dev") and a derived one ("codereview").
    let group_colors = rows[0]["groupColors"]
        .as_object()
        .expect("groupColors is an object");
    assert_eq!(
        group_colors.get("dev"),
        Some(&serde_json::json!("#0a84ff")),
        "configured color round-trips onto the wire: {committed}"
    );
    assert!(
        group_colors
            .get("codereview")
            .and_then(|v| v.as_str())
            .is_some_and(|hex| hex.starts_with('#') && hex.len() == 7),
        "derived color is still a real hex value on the wire: {committed}"
    );

    // Public repo: the fixture carries no real account data. A positive
    // control on the same probe — the fake domain really is present — so an
    // empty match cannot read as "clean".
    assert!(
        committed.contains("@example.com"),
        "positive control: the fixture uses example.com addresses"
    );
    for row in &rows {
        let name = row["name"].as_str().expect("name is a string");
        let email = name.strip_suffix("/codex").unwrap_or(name);
        assert!(
            email.ends_with("@example.com"),
            "every fixture account is obviously fake: {name}"
        );
    }
}

// ---------------------------------------------------------------------
// The canonical Codex fixtures TcrBar decodes.
// ---------------------------------------------------------------------

/// `groups` on the JSON path: an array for a labelled account (including
/// multi-membership), and `[]` — never `null` — for an unlabelled one. This
/// is config, not a serving counter, so it does not get the
/// null-on-offline treatment other fields get.
#[test]
fn groups_render_as_array_never_null() {
    let (snapshot, thresholds) = status_contract_snapshot();
    let json = status_json(&snapshot, &thresholds, StatusSource::Live);
    let alice = status_row(&json, "alice@example.com");
    assert_eq!(
        alice["groups"],
        serde_json::json!(["codereview", "dev"]),
        "labelled account carries its groups: {alice}"
    );
    let dave = status_row(&json, "dave@example.com");
    assert_eq!(
        dave["groups"],
        serde_json::json!([]),
        "unlabelled account renders [], never null: {dave}"
    );
}

/// `groupColors` is FLEET-wide, repeated on every row exactly like
/// `http1Only` — not filtered down to the groups the row itself belongs
/// to. Dave's row carries no groups but must still see `codereview`'s
/// color, and a group nobody on this fleet is even labelled with (`ops`)
/// still appears: the map is whatever the caller passes, not derived
/// per-row from `groups`.
#[test]
fn group_colors_is_fleet_wide_not_filtered_to_the_rows_own_groups() {
    let (snapshot, thresholds) = status_contract_snapshot();
    let group_colors: std::collections::BTreeMap<String, String> = [
        ("codereview".to_string(), "#5163d6".to_string()),
        ("dev".to_string(), "#0a84ff".to_string()),
        ("ops".to_string(), "#ff9500".to_string()),
    ]
    .into_iter()
    .collect();
    let json = render_accounts_json(
        &snapshot,
        &thresholds,
        StatusSource::Live,
        None,
        false,
        None,
        &group_colors,
    );
    let dave = status_row(&json, "dave@example.com");
    assert_eq!(
        dave["groups"],
        serde_json::json!([]),
        "dave carries no groups of his own"
    );
    assert_eq!(
        dave["groupColors"],
        serde_json::json!({
            "codereview": "#5163d6",
            "dev": "#0a84ff",
            "ops": "#ff9500",
        }),
        "dave's row still carries the full fleet map, including groups \
             he does not belong to: {dave}"
    );
}

/// `groupColors` on a fleet with no groups at all: `{}`, never `null` —
/// same never-`null` contract `groups`/`reservedGroups` already carry.
#[test]
fn group_colors_is_empty_object_never_null_when_no_groups() {
    let (snapshot, thresholds) = status_contract_snapshot();
    let json = render_accounts_json(
        &snapshot,
        &thresholds,
        StatusSource::Live,
        None,
        false,
        None,
        &std::collections::BTreeMap::new(),
    );
    let rows: Vec<serde_json::Value> = serde_json::from_str(&json).expect("valid json");
    for row in &rows {
        assert_eq!(
            row["groupColors"],
            serde_json::json!({}),
            "empty fleet groups render as {{}}, never null: {row}"
        );
    }
}

/// `groups=` on the text path: a greppable `groups=codereview,dev` token for
/// a labelled account, and the token omitted entirely (not `groups=`) for an
/// unlabelled one — the same "nothing to say" idiom as `fable=` and
/// `last_stream_error=`.
#[test]
fn groups_text_line_greppable_and_omitted_when_empty() {
    let (snapshot, _thresholds) = status_contract_snapshot();
    let text = render_accounts(&snapshot, StatusSource::Live);
    let alice_line = text
        .lines()
        .find(|l| l.contains("alice@example.com"))
        .expect("alice line");
    assert!(
        alice_line.contains("groups=codereview,dev"),
        "labelled account's groups are greppable: {alice_line}"
    );
    let dave_line = text
        .lines()
        .find(|l| l.contains("dave@example.com"))
        .expect("dave line");
    assert!(
        !dave_line.contains("groups="),
        "unlabelled account omits the token entirely: {dave_line}"
    );
}

/// `plan=` on the text path: the derived label, greppable, on every account
/// that has been profiled — and the token omitted ENTIRELY, never
/// `plan=unknown`, for one that has not. This is the only token that tells
/// two rows sharing an email apart, so a fabricated value is worse than
/// silence.
#[test]
fn plan_text_line_greppable_and_omitted_when_never_profiled() {
    let (snapshot, _thresholds) = status_contract_snapshot();
    let text = render_accounts(&snapshot, StatusSource::Live);
    let line_for = |name: &str| {
        text.lines()
            .find(|l| l.contains(name))
            .unwrap_or_else(|| panic!("{name} line"))
            .to_string()
    };

    let alice = line_for("alice@example.com");
    assert!(
        alice.contains("plan=Max 20x"),
        "the multiplier suffix is what a Max row shows: {alice}"
    );
    let bob = line_for("bob@example.com");
    assert!(
        bob.contains("plan=Team 5x"),
        "a premium Team seat shows its multiplier, not `team_tier_1`: {bob}"
    );
    let carol = line_for("carol@example.com");
    assert!(
        carol.contains("plan=Team Standard"),
        "a standard seat's tier names no multiplier, so the seat shows: {carol}"
    );
    let dave = line_for("dave@example.com");
    assert!(
        !dave.contains("plan="),
        "a never-profiled account omits the token entirely: {dave}"
    );
}

/// The same three facts on the JSON path, plus the raw provider strings a
/// script keys on and the `orgUuid` it narrows an ambiguous name with.
#[test]
fn plan_and_org_render_on_the_json_path() {
    let (snapshot, thresholds) = status_contract_snapshot();
    let json = render_accounts_json(
        &snapshot,
        &thresholds,
        StatusSource::Live,
        None,
        false,
        None,
        &std::collections::BTreeMap::new(),
    );
    let rows: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");

    assert_eq!(rows[0]["plan"], "Max 20x");
    assert_eq!(rows[0]["organizationType"], "claude_max");
    assert_eq!(rows[0]["rateLimitTier"], "default_claude_max_20x");
    assert!(
        rows[0]["seatTier"].is_null(),
        "a Max row has no seat — null, not an empty string"
    );
    assert_eq!(rows[0]["orgUuid"], "11111111-1111-1111-1111-111111111111");

    assert_eq!(rows[1]["plan"], "Team 5x");
    assert_eq!(rows[1]["seatTier"], "team_tier_1");
    assert_eq!(rows[2]["plan"], "Team Standard");
    assert_eq!(
        rows[1]["orgUuid"], rows[2]["orgUuid"],
        "two rows in one org report the same orgUuid — what a client narrows by"
    );

    assert!(
        rows[3]["plan"].is_null() && rows[3]["organizationType"].is_null(),
        "a never-profiled row publishes null, never a fabricated default"
    );
    assert!(
        rows[3]["orgUuid"].is_null(),
        "and no org to narrow by either"
    );
}

/// The plan is a CONFIG fact, not a serving counter, so it is real on the
/// offline path too — unlike `requests`/`streamErrorCount`, which are null
/// there. A `plan` that vanished whenever the proxy was down would be
/// useless in exactly the moment an operator is reading `tcr status`.
#[test]
fn plan_is_reported_on_the_offline_path_too() {
    let (snapshot, thresholds) = status_contract_snapshot();
    let json = render_accounts_json(
        &snapshot,
        &thresholds,
        StatusSource::Offline,
        None,
        false,
        None,
        &std::collections::BTreeMap::new(),
    );
    let rows: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
    assert_eq!(rows[0]["plan"], "Max 20x");
    assert!(
        rows[0]["requests"].is_null(),
        "the control: a real serving counter IS null offline, so this test is \
             not simply reading a path where nothing is nulled"
    );
}

/// `sevenDayOiState` and `sevenDayOiResetAtMs` render from a snapshot that
/// carries the Fable weekly pair, and both keys are `null` — never absent,
/// never a fabricated `0`/`"ok"` — for an account whose window was never
/// learned. Mirrors `sevenDayState`/`sevenDayResetAtMs`'s own contract.
#[test]
fn fable_weekly_state_and_reset_render_and_null_when_unlearned() {
    let (snapshot, thresholds) = reserved_groups_snapshot();
    let mut snapshot = snapshot;
    let learned_reset = fixture_instant(1_767_312_000_000);
    // alice: learned, above threshold → "near". dave: never learned → null.
    snapshot.accounts[0].seven_day_oi = Some(0.95);
    snapshot.accounts[0].seven_day_oi_reset = Some(learned_reset);
    snapshot.accounts[1].seven_day_oi = None;
    snapshot.accounts[1].seven_day_oi_reset = None;

    let json = status_json(&snapshot, &thresholds, StatusSource::Live);
    let alice = status_row(&json, "alice@example.com");
    assert_eq!(
        alice["sevenDayOiState"],
        serde_json::json!("near"),
        "learned, above-threshold window renders its state: {alice}"
    );
    assert_eq!(
        alice["sevenDayOiResetAtMs"],
        serde_json::json!(learned_reset.unix_timestamp_nanos() as i64 / 1_000_000),
        "learned window's reset reaches the wire: {alice}"
    );
    let dave = status_row(&json, "dave@example.com");
    assert_eq!(
        dave["sevenDayOiState"],
        serde_json::Value::Null,
        "never-learned window's state is null, not absent: {dave}"
    );
    assert_eq!(
        dave["sevenDayOiResetAtMs"],
        serde_json::Value::Null,
        "never-learned window's reset is null, not absent: {dave}"
    );
}

/// `reservedGroups` on the JSON wire: always an array, `[]` — never
/// `null` — when nothing is reserved, mirroring `groups`'s own contract.
/// Bridge test #7.
fn reserved_groups_snapshot() -> (StatsSnapshot, Vec<f64>) {
    let base = AccountSnapshot {
        slot: 0,
        name: String::new(),
        provider: Provider::Anthropic,
        organization_type: None,
        rate_limit_tier: None,
        seat_tier: None,
        plan_display_name: None,
        org_uuid: None,
        org_name: None,
        priority: 0,
        status: "active".to_string(),
        disabled: false,
        five_hour: None,
        five_hour_reset: None,
        five_hour_absent_on_plan: false,
        seven_day: None,
        seven_day_reset: None,
        seven_day_oi: None,
        seven_day_oi_reset: None,
        requests: 0,
        input_tokens: 0,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
        last_used: None,
        rate_limited_until: None,
        probe_status: ProbeStatus::Never,
        last_probe: None,
        quota_updated_at: None,
        probe_error: None,
        quota_state: QuotaState::Normal,
        gate: crate::records::stats::GateReason::Ok,
        free_at: None,
        stream_error_count: 0,
        last_stream_error: None,
        groups: Vec::new(),
        reserved_groups: Vec::new(),
        parked_groups: Vec::new(),
        control_allowed_groups: Vec::new(),
        usage: None,
        codex: Default::default(),
    };
    let accounts = vec![
        AccountSnapshot {
            name: "alice@example.com".to_string(),
            gate: crate::records::stats::GateReason::Reserved,
            groups: vec!["dev".to_string(), "codereview".to_string()],
            reserved_groups: vec!["codereview".to_string()],
            parked_groups: Vec::new(),
            control_allowed_groups: Vec::new(),
            usage: None,
            ..base.clone()
        },
        AccountSnapshot {
            name: "dave@example.com".to_string(),
            ..base
        },
    ];
    (
        StatsSnapshot {
            accounts,
            current: None,
            current_by_provider: crate::records::stats::CurrentByProvider::default(),
            recent: Vec::new(),
            sessions: Vec::new(),
            wire_sessions: Vec::new(),
            wire_sessions_summary: tcr_status_wire::SessionsSummary::default(),
        },
        vec![0.90, 0.90],
    )
}

#[test]
fn reserved_groups_render_as_array_never_null() {
    let (snapshot, thresholds) = reserved_groups_snapshot();
    let json = status_json(&snapshot, &thresholds, StatusSource::Live);
    let alice = status_row(&json, "alice@example.com");
    assert_eq!(
        alice["reservedGroups"],
        serde_json::json!(["codereview"]),
        "reserved subset of alice's groups: {alice}"
    );
    assert_eq!(alice["gate"], serde_json::json!("reserved"));
    let dave = status_row(&json, "dave@example.com");
    assert_eq!(
        dave["reservedGroups"],
        serde_json::json!([]),
        "nothing reserved renders [], never null: {dave}"
    );
}

/// `reservedGroups` round-trips through the `status.rs` wire (the
/// full server->client payload, not just this file's JSON renderer) —
/// bridge test #7's other half.
#[test]
fn reserved_groups_round_trip_through_status_payload() {
    let (snapshot, thresholds) = reserved_groups_snapshot();
    let payload =
        StatusPayload::from_snapshot(&snapshot, &thresholds, false, None, Default::default());
    let wire = serde_json::to_string(&payload).expect("serialize");
    assert!(
        wire.contains("\"reservedGroups\":[\"codereview\"]"),
        "reservedGroups is camelCase on the wire: {wire}"
    );
    let back: StatusPayload = serde_json::from_str(&wire).expect("deserialize");
    let (rebuilt, _) = back.into_snapshot();
    assert_eq!(
        rebuilt.accounts[0].reserved_groups,
        vec!["codereview".to_string()]
    );
    assert_eq!(rebuilt.accounts[1].reserved_groups, Vec::<String>::new());
}

/// The same two surfaces for `parkedGroups`: an array on the JSON wire —
/// `[]`, never `null`, when nothing is parked — the `"parked"` gate token
/// beside it, and a full round trip through the server→client payload.
/// Both halves in one test because they are one contract: the panel reads
/// the field to name the group, and the token to know the row is out.
#[test]
fn parked_groups_render_as_an_array_and_round_trip_with_their_gate() {
    let (mut snapshot, thresholds) = reserved_groups_snapshot();
    snapshot.accounts[0].gate = crate::records::stats::GateReason::Parked;
    snapshot.accounts[0].reserved_groups = Vec::new();
    snapshot.accounts[0].parked_groups = vec!["codereview".to_string()];

    let json = status_json(&snapshot, &thresholds, StatusSource::Live);
    let alice = status_row(&json, "alice@example.com");
    assert_eq!(
        alice["parkedGroups"],
        serde_json::json!(["codereview"]),
        "parked subset of alice's groups: {alice}"
    );
    assert_eq!(alice["gate"], serde_json::json!("parked"));
    let dave = status_row(&json, "dave@example.com");
    assert_eq!(
        dave["parkedGroups"],
        serde_json::json!([]),
        "nothing parked renders [], never null: {dave}"
    );

    let payload =
        StatusPayload::from_snapshot(&snapshot, &thresholds, false, None, Default::default());
    let wire = serde_json::to_string(&payload).expect("serialize");
    assert!(
        wire.contains("\"parkedGroups\":[\"codereview\"]"),
        "parkedGroups is camelCase on the wire: {wire}"
    );
    let back: StatusPayload = serde_json::from_str(&wire).expect("deserialize");
    let (rebuilt, _) = back.into_snapshot();
    assert_eq!(
        rebuilt.accounts[0].parked_groups,
        vec!["codereview".to_string()]
    );
    assert_eq!(
        rebuilt.accounts[0].gate,
        crate::records::stats::GateReason::Parked
    );
    assert_eq!(rebuilt.accounts[1].parked_groups, Vec::<String>::new());
}

/// A server that predates the field sends no `parkedGroups` key at all, and
/// its rows must still decode — `#[serde(default)]`'s whole job. Same
/// forward-compat guard `reservedGroups` carries.
#[test]
fn a_payload_without_parked_groups_still_deserializes() {
    let (snapshot, thresholds) = reserved_groups_snapshot();
    let payload =
        StatusPayload::from_snapshot(&snapshot, &thresholds, false, None, Default::default());
    let mut wire: serde_json::Value = serde_json::to_value(&payload).expect("payload serializes");
    for account in wire["accounts"]
        .as_array_mut()
        .expect("accounts is an array")
    {
        account
            .as_object_mut()
            .expect("account row is an object")
            .remove("parkedGroups")
            .expect("the field was there to remove — the positive control");
    }
    let back: StatusPayload =
        serde_json::from_value(wire).expect("a row with no parkedGroups key still decodes");
    let (rebuilt, _) = back.into_snapshot();
    assert_eq!(rebuilt.accounts[0].parked_groups, Vec::<String>::new());
}
