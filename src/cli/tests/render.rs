//! Tests for `cli/render.rs`: the offline snapshot, the per-account text and JSON rows,
//! held windows and reset countdowns.

use super::*;
#[tokio::test]
async fn render_accounts_emits_one_greppable_line_per_account() {
    let config = load_from(TWO_ACCOUNTS);
    let snapshot = snapshot_offline(
        config,
        Arc::new(NoRefresh),
        Arc::new(FixedProber { util: 0.25 }),
        true,
    )
    .await;
    // Offline is the honest source for a snapshot built by `snapshot_offline`,
    // and it is the exact line shape `tcr accounts` emits — that verb is
    // offline by construction, so this is the greppable line a human reads
    // most often.
    let text = render_accounts(&snapshot, StatusSource::Offline);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "one line per account");
    // Each line is greppable: name + priority + per-window utilization.
    assert!(lines[0].contains("alice@example.com"));
    assert!(lines[0].contains("priority=0"));
    // FixedProber sets five_hour=0.25, seven_day/seven_day_oi=None.
    assert!(lines[0].contains("5h=25%"), "5h window: {}", lines[0]);
    assert!(lines[0].contains("wk=n/a"), "wk unknown: {}", lines[0]);
    // seven_day_oi is None → the fable token is omitted entirely.
    assert!(!lines[0].contains("fable="), "no fable token: {}", lines[0]);
    // Well under the 0.9 default threshold → an honest `ok` state.
    assert!(lines[0].contains("state=ok"), "healthy state: {}", lines[0]);
    assert!(lines[1].contains("bob@example.com"));
    assert!(lines[1].contains("priority=1"));
    // At 25% util no window is a binding hold, but it still has a live
    // reset — and a window's reset is a property of the window, not of
    // whether it is pinning the account, so the inline countdown still
    // shows. The legacy `held=`/`quota=`/`quota_state=` fields are gone
    // for good.
    assert!(
        text.contains("5h=25%(+"),
        "under-threshold accounts still carry their window's countdown: {text}"
    );
    assert!(
        !text.contains("held=") && !text.contains("quota="),
        "legacy quota/held fields are gone: {text}"
    );
    // Every token stays `key=value`, so the new stream-error token is greppable
    // wherever it sits in the line and does not displace its neighbours.
    for line in &lines {
        assert!(
            line.contains("probe=") && line.contains("stream_errors=n/a"),
            "the offline line keeps probe= and carries an unmeasured \
                 stream-error token: {line}"
        );
    }
}

/// THE HONESTY TEST. An offline snapshot's serving counters live in the
/// SERVER's process, so `input_tokens` here is structurally zero — there is
/// nothing to divide by and no measurement was taken. It must therefore emit
/// `null` (JSON) / `n/a` (human), never a `0.0` that reads as a measured "0%
/// cache hits".
///
/// That false zero is the entire reason this bug hid: `tcr status --json`
/// reported `"cacheHitRatio": 0.0` for every account forever, so the one
/// surface that should have shown a prompt-cache catastrophe reported
/// "cache fine" straight through it. The `source` field is the other half —
/// it names the process the counters would have come from.
#[tokio::test]
async fn offline_status_reports_null_not_zero_hit_ratio() {
    let config = load_from(TWO_ACCOUNTS);
    let thresholds = config.switch_thresholds();
    let snapshot = snapshot_offline(
        config,
        Arc::new(NoRefresh),
        Arc::new(FixedProber { util: 0.25 }),
        true,
    )
    .await;

    let json = status_json(&snapshot, &thresholds, StatusSource::Offline);
    let rows: Vec<serde_json::Value> = serde_json::from_str(&json).expect("valid json");
    assert_eq!(rows.len(), 2, "one row per account");
    for (i, row) in rows.iter().enumerate() {
        assert_eq!(
            snapshot.accounts[i].input_tokens, 0,
            "the premise: an offline snapshot has counted nothing"
        );
        // The four serving counters read `null` on the offline path, not
        // a real `0` — same false-zero fix as `cacheHitRatio` below, one
        // field family over: these live in the SERVING process, so a
        // fresh offline `Manager` has structurally never measured them.
        for key in ["requests", "inputTokens", "outputTokens", "cacheReadTokens"] {
            assert!(
                row[key].is_null(),
                "{key} is unmeasured on the offline path, never a false 0: {row}"
            );
            assert_ne!(
                row[key],
                serde_json::json!(0),
                "the literal 0 that would masquerade as a measurement is gone: {row}"
            );
        }
        assert!(
            row["cacheHitRatio"].is_null(),
            "an uncounted ratio is null, not a number: {row}"
        );
        assert_ne!(
            row["cacheHitRatio"],
            serde_json::json!(0.0),
            "the literal 0.0 that masqueraded as a measurement is gone: {row}"
        );
        assert_eq!(
            row["source"], "offline",
            "every row names the process its counters came from: {row}"
        );
    }

    // A ratio that IS measured still renders as a number — the null is about
    // absence of data, not a blanket suppression. `requests` must clear
    // `CACHE_SIGNAL_FLOOR_REQUESTS` too, or the floor added below would
    // null this out regardless of the token counts.
    let mut counted = snapshot.clone();
    counted.accounts[0].requests = CACHE_SIGNAL_FLOOR_REQUESTS;
    counted.accounts[0].input_tokens = 1_000;
    counted.accounts[0].cache_read_tokens = 750;
    let live = status_json(&counted, &thresholds, StatusSource::Live);
    let rows: Vec<serde_json::Value> = serde_json::from_str(&live).expect("valid json");
    assert_eq!(rows[0]["cacheHitRatio"], serde_json::json!(0.75));
    assert_eq!(rows[0]["source"], "live");
    assert!(
        rows[1]["cacheHitRatio"].is_null(),
        "the still-uncounted account stays null: {}",
        rows[1]
    );

    // The human view uses the same `n/a` idiom the unknown quota windows use.
    let text = render_accounts(&snapshot, StatusSource::Offline);
    for line in text.lines() {
        assert!(
            line.contains("cache=n/a"),
            "uncounted cache reads as n/a, never 0%: {line}"
        );
        assert!(!line.contains("cache=0%"), "no false measured zero: {line}");
    }
    assert!(
        render_accounts(&counted, StatusSource::Live).contains("cache=75%"),
        "a measured ratio still renders as a percentage"
    );
}

/// THE SIGNAL-FLOOR TEST. An account whose only real traffic is a
/// handful of cold diverts has genuinely nonzero, genuinely LIVE token
/// counters — this is not the `input_tokens == 0` case above — but too
/// few served requests to trust the ratio it would produce. Before this
/// fix, `tcr status` reported that account's near-empty-denominator ratio
/// as a confident measured number (as low as single digits of percent),
/// which read exactly like "this account's cache is broken" and sent a
/// lead chasing a regression that was never there
/// This pins the fix: under `CACHE_SIGNAL_FLOOR_REQUESTS`, the ratio is
/// `null`/`n/a` — no-signal — same as the honest-zero case, never a
/// number.
#[tokio::test]
async fn an_account_under_the_signal_floor_reports_no_signal_not_a_number() {
    let config = load_from(TWO_ACCOUNTS);
    let thresholds = config.switch_thresholds();
    let snapshot = snapshot_offline(
        config,
        Arc::new(NoRefresh),
        Arc::new(FixedProber { util: 0.25 }),
        true,
    )
    .await;

    // Real, live, nonzero tokens — a genuine measurement, not an unset
    // counter — but ONE below the floor's request count. A single cold
    // divert can plausibly write tens of thousands of cache_creation
    // tokens in one request, so a small `requests` count with a large
    // token count is exactly the shape this guards, not a fixture
    // artefact.
    let mut sparse = snapshot;
    sparse.accounts[0].requests = CACHE_SIGNAL_FLOOR_REQUESTS - 1;
    sparse.accounts[0].input_tokens = 42_000;
    sparse.accounts[0].cache_read_tokens = 100;

    let live = status_json(&sparse, &thresholds, StatusSource::Live);
    let rows: Vec<serde_json::Value> = serde_json::from_str(&live).expect("valid json");
    assert!(
        rows[0]["cacheHitRatio"].is_null(),
        "under the floor, a real nonzero ratio still reads as no-signal, \
             not a misleadingly precise number: {}",
        rows[0]
    );
    assert_ne!(
        rows[0]["cacheHitRatio"],
        serde_json::json!(0.0),
        "no-signal must never collapse to the false '0% cache hits' this \
             module already fixed once: {}",
        rows[0]
    );

    let text = render_accounts(&sparse, StatusSource::Live);
    let line = text
        .lines()
        .find(|l| l.contains(&sparse.accounts[0].name))
        .expect("the sparse account's line");
    assert!(
        line.contains("cache=n/a"),
        "the human view reports the same no-signal state: {line}"
    );

    // One request OVER the floor, same tokens: the ratio is measured and
    // renders as the real number — this is the boundary, not a blanket
    // suppression of light accounts.
    let mut at_floor = sparse.clone();
    at_floor.accounts[0].requests = CACHE_SIGNAL_FLOOR_REQUESTS;
    let live = status_json(&at_floor, &thresholds, StatusSource::Live);
    let rows: Vec<serde_json::Value> = serde_json::from_str(&live).expect("valid json");
    assert_eq!(
        rows[0]["cacheHitRatio"],
        serde_json::json!(100.0 / 42_000.0),
        "at the floor the same tokens now render as a real measurement: {}",
        rows[0]
    );
}

/// The stream-error counter obeys the same rule as the cache ratio above, and
/// for a sharper reason: `0` on an ERROR counter is an affirmative all-clear.
/// Offline it is structurally zero — the count lives in the serving process —
/// so publishing it as `0` would claim "no truncated streams" about a process
/// this one never spoke to.
#[tokio::test]
async fn offline_status_reports_null_not_zero_stream_errors() {
    let config = load_from(TWO_ACCOUNTS);
    let thresholds = config.switch_thresholds();
    let snapshot = snapshot_offline(
        config,
        Arc::new(NoRefresh),
        Arc::new(FixedProber { util: 0.25 }),
        true,
    )
    .await;

    let offline = status_json(&snapshot, &thresholds, StatusSource::Offline);
    let rows: Vec<serde_json::Value> = serde_json::from_str(&offline).expect("valid json");
    for row in &rows {
        // `get`, not `row[...]`: indexing a MISSING key also yields Null, so
        // `row["streamErrorCount"].is_null()` would pass against the very bug
        // this test exists for — the field never being rendered at all.
        assert_eq!(
            row.get("streamErrorCount"),
            Some(&serde_json::Value::Null),
            "the key is present AND null — unmeasured, never a 0 all-clear: {row}"
        );
        assert_eq!(
            row.get("lastStreamError"),
            Some(&serde_json::Value::Null),
            "nothing measured: {row}"
        );
    }

    // Measured and genuinely clean is a DIFFERENT state, and renders as 0.
    let live = status_json(&snapshot, &thresholds, StatusSource::Live);
    let rows: Vec<serde_json::Value> = serde_json::from_str(&live).expect("valid json");
    assert_eq!(rows[0]["streamErrorCount"], serde_json::json!(0));

    // Measured and dirty carries both the count and the latest error type.
    let mut errored = snapshot.clone();
    errored.accounts[0].stream_error_count = 3;
    errored.accounts[0].last_stream_error = Some("overloaded_error".to_string());
    let live = status_json(&errored, &thresholds, StatusSource::Live);
    let rows: Vec<serde_json::Value> = serde_json::from_str(&live).expect("valid json");
    assert_eq!(rows[0]["streamErrorCount"], serde_json::json!(3));
    assert_eq!(
        rows[0]["lastStreamError"],
        serde_json::json!("overloaded_error")
    );

    // The text view carries the same three states as greppable tokens.
    let text = render_accounts(&snapshot, StatusSource::Offline);
    for line in text.lines() {
        assert!(
            line.contains("stream_errors=n/a"),
            "unmeasured reads n/a, never 0: {line}"
        );
        assert!(
            !line.contains("stream_errors=0"),
            "no false all-clear: {line}"
        );
    }
    assert!(
        render_accounts(&snapshot, StatusSource::Live).contains("stream_errors=0"),
        "a measured clean fleet still renders a real 0"
    );
    let text = render_accounts(&errored, StatusSource::Live);
    assert!(
        text.contains("stream_errors=3"),
        "the count is greppable: {text}"
    );
    assert!(
        text.contains("last_stream_error=overloaded_error"),
        "the latest error type is greppable: {text}"
    );
}

/// END-TO-END through the PRODUCTION path, which is the claim that actually
/// matters: a real hybrid listener (the thing that injects `ClientAddr`), a
/// real HTTP round trip on a real socket, and the real client parser.
///
/// The counters below exist ONLY in the served manager's process — exactly the
/// situation that made `tcr status` lie, since the offline path builds a fresh
/// `Manager` that can never see them. Binds port 0 (a free ephemeral port) and
/// overrides the config's 3456, so this never touches a real running proxy.
#[tokio::test]
async fn live_status_reads_the_running_servers_counters() {
    let (listener, port) = crate::test_support::loopback::listener().await;
    let mut config = load_from(TWO_ACCOUNTS);
    config.proxy.port = port;

    let manager = crate::test_support::live_proxy::serve(listener, config.clone(), None);
    // 750 of 1000 input tokens were prompt-cache reads: a real 75% hit ratio,
    // held in this process only. `record_served` clears
    // `CACHE_SIGNAL_FLOOR_REQUESTS` so the ratio below is a reported number,
    // not the new floor's no-signal null — this test is about the ratio
    // crossing the wire from the live server, not about the floor itself.
    manager.update_usage(0, 1_000, 200, 750, 50);
    let now = OffsetDateTime::now_utc();
    for _ in 0..CACHE_SIGNAL_FLOOR_REQUESTS {
        manager.record_served(0, now, None, crate::stats::SessionKind::Fallback);
    }

    let payload = match fetch_live_status(&config).await {
        Ok(p) => p,
        Err(LiveStatusError::NoServer) => panic!("the spawned server did not answer"),
        Err(err) => panic!("live status unusable: {}", err.why()),
    };
    let build = payload.build.clone();
    let (snapshot, thresholds) = payload.into_snapshot();

    let json = render_accounts_json(
        &snapshot,
        &thresholds,
        StatusSource::Live,
        Some(&build),
        false,
        None,
        &Default::default(),
    );
    let rows: Vec<serde_json::Value> = serde_json::from_str(&json).expect("valid json");
    assert_eq!(
        rows[0]["cacheHitRatio"],
        serde_json::json!(0.75),
        "the ratio is the SERVER's real measurement, not a structural zero: {}",
        rows[0]
    );
    assert_eq!(rows[0]["inputTokens"], 1_000);
    assert_eq!(rows[0]["cacheReadTokens"], 750);
    assert_eq!(rows[0]["source"], "live");
    // The account that served nothing still reports an honest null, not a 0.0.
    assert!(rows[1]["cacheHitRatio"].is_null(), "{}", rows[1]);
    // Thresholds came from the SERVER, not from re-reading the config file.
    assert_eq!(thresholds.len(), 2);

    // END-TO-END on the build stamp too: the sha crossed a real socket from
    // the serving process, and reaches every rendered row.
    assert_eq!(
        build,
        BuildInfo::current(),
        "the served payload names the build that served it"
    );
    for row in &rows {
        assert_eq!(row["serverSha"], serde_json::json!(build_info::SHA));
    }
}

/// A Codex account reads as one on both `tcr status` surfaces, through the LIVE
/// path the TcrBar panel polls: its JSON row carries `provider: "codex"` and the
/// ChatGPT plan label, and its text line names the same plan. The Anthropic row
/// beside it carries no `provider` key at all — what keeps a panel that predates
/// the key decoding it unchanged — and keeps its Anthropic label.
///
/// Through a real socket from a running server, not from a hand-built snapshot:
/// the provider has to survive `Manager::snapshot`, the server's `StatusPayload`
/// and the client's `into_snapshot` before a renderer sees it, and the contract
/// fixture renders a snapshot directly, so it sees none of those three hops.
/// Binds port 0, never the config's 3456.
#[tokio::test]
async fn live_status_shows_a_codex_accounts_provider_and_chatgpt_plan() {
    let (listener, port) = crate::test_support::loopback::listener().await;
    let mut config = load_from(
        r#"{
              "accounts": [
                { "name": "alice@example.com", "type": "oauth", "accessToken": "at-a",
                  "refreshToken": "rt-a", "expiresAt": 1893456000000, "priority": 0,
                  "organizationType": "claude_max", "rateLimitTier": "default_claude_max_20x" },
                { "name": "alice@example.com/codex", "type": "oauth", "provider": "codex",
                  "accountUuid": "11111111-1111-4111-8111-111111111111",
                  "organizationType": "pro", "accessToken": "at-c", "refreshToken": "rt-c",
                  "expiresAt": 1893456000000, "priority": 1 }
              ]
            }"#,
    );
    config.proxy.port = port;
    crate::test_support::live_proxy::serve(listener, config.clone(), None);

    let payload = match fetch_live_status(&config).await {
        Ok(p) => p,
        Err(LiveStatusError::NoServer) => panic!("the spawned server did not answer"),
        Err(err) => panic!("live status unusable: {}", err.why()),
    };
    let (snapshot, thresholds) = payload.into_snapshot();

    let json = status_json(&snapshot, &thresholds, StatusSource::Live);
    let rows: Vec<serde_json::Value> = serde_json::from_str(&json).expect("valid json");
    let (claude, codex) = (&rows[0], &rows[1]);
    assert_eq!(codex["provider"], "codex", "{codex}");
    assert_eq!(codex["plan"], "ChatGPT Pro", "{codex}");
    assert_eq!(
        codex["organizationType"], "pro",
        "the raw claim still rides beside the label: {codex}"
    );
    assert!(
        claude.get("provider").is_none(),
        "an Anthropic row carries no provider key, not even null: {claude}"
    );
    assert_eq!(claude["plan"], "Max 20x", "{claude}");

    let text = render_accounts(&snapshot, StatusSource::Live);
    let line_for = |name: &str| {
        text.lines()
            .find(|l| l.starts_with(&format!("account {name} ")))
            .unwrap_or_else(|| panic!("no line for {name}:\n{text}"))
    };
    let codex_line = line_for("alice@example.com/codex");
    assert!(codex_line.contains("plan=ChatGPT Pro"), "{codex_line}");
    let claude_line = line_for("alice@example.com");
    assert!(claude_line.contains("plan=Max 20x"), "{claude_line}");
}

/// The offline path has no serving process, so `serverSha` is `null` — the
/// same "not measured" idiom as `cacheHitRatio`, and never this CLI's own sha
/// standing in for a server's.
#[tokio::test]
async fn offline_rows_report_a_null_server_sha() {
    let config = load_from(TWO_ACCOUNTS);
    let thresholds = config.switch_thresholds();
    let snapshot = snapshot_offline(
        config,
        Arc::new(NoRefresh),
        Arc::new(FixedProber { util: 0.25 }),
        false,
    )
    .await;

    let json = status_json(&snapshot, &thresholds, StatusSource::Offline);
    let rows: Vec<serde_json::Value> = serde_json::from_str(&json).expect("valid json");
    for row in &rows {
        assert!(
            row["serverSha"].is_null(),
            "no server answered, so no server sha: {row}"
        );
        assert!(row["serverDirty"].is_null(), "{row}");
    }
}

#[tokio::test]
async fn status_offline_does_not_persist_the_config() {
    // A scripted prober returns usage; snapshot_offline builds a manager with
    // config_path = None, so nothing is written back — the temp config must
    // be byte-unchanged after a full probe sweep.
    let path = write_config("no-persist", TWO_ACCOUNTS);
    let before = fs::read_to_string(&path).unwrap();
    let config = load(&path);
    let _snapshot = snapshot_offline(
        config,
        Arc::new(NoRefresh),
        Arc::new(FixedProber { util: 0.5 }),
        true,
    )
    .await;
    let after = fs::read_to_string(&path).unwrap();
    assert_eq!(
        before, after,
        "an offline status probe must never persist to the config file"
    );
    fs::remove_file(&path).ok();
}

// --- `tcr status --check` ----------------------------------------------

/// An INSTRUMENTED refresher: records every refresh attempt and, like
/// [`NoRefresh`], never contacts the network. Injected into `snapshot_offline`
/// to prove the offline snapshot routes an expired account's refresh through the
/// caller's refresher — never a hidden [`crate::oauth::LiveRefresher`] that would
/// hit the OAuth endpoint and revoke the live server's single-use token.
struct RecordingRefresher {
    calls: Arc<AtomicU64>,
}
impl TokenRefresher for RecordingRefresher {
    fn refresh(&self, _provider: Provider, _refresh_token: String) -> RefreshFuture {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Err(OAuthError::Transient(
                "recorded — never networked".to_string(),
            ))
        })
    }
}

#[tokio::test]
async fn offline_snapshot_over_expired_account_routes_through_injected_refresher() {
    // Biting test for Fix 1: over an EXPIRED account, the offline snapshot must
    // hand the refresh to the INJECTED refresher (production wires NoRefresh,
    // which never networks) rather than a hidden LiveRefresher. A recording
    // refresher proves the routing — if snapshot_offline ignored its refresher
    // param and hardcoded LiveRefresher, `calls` would stay 0 and this fails.
    // The row must still surface a clear, greppable probe=error, and the config
    // stays byte-identical.
    let path = write_config("expired", EXPIRED_ACCOUNT);
    let before = fs::read_to_string(&path).unwrap();
    let config = load(&path);
    let calls = Arc::new(AtomicU64::new(0));
    let refresher = Arc::new(RecordingRefresher {
        calls: Arc::clone(&calls),
    });
    let snapshot = snapshot_offline(config, refresher, Arc::new(RejectingProber), true).await;
    let text = render_accounts(&snapshot, StatusSource::Offline);
    assert!(
        text.contains("stale@example.com"),
        "renders the account: {text}"
    );
    assert!(
        text.contains("probe=error"),
        "surfaces a clear probe status for the expired account: {text}"
    );
    assert!(
        calls.load(Ordering::SeqCst) >= 1,
        "the expired account's refresh was routed through the injected refresher, \
             never a hidden LiveRefresher (which would have hit the OAuth endpoint)"
    );
    let after = fs::read_to_string(&path).unwrap();
    assert_eq!(
        before, after,
        "an offline probe of an expired account must never persist"
    );
    fs::remove_file(&path).ok();
}

// --- reset countdowns (3a) ---------------------------------------------

#[test]
fn reset_countdown_tiers_minutes_then_hours_then_days() {
    let now = OffsetDateTime::now_utc();
    use time::Duration;
    // Minutes under two hours (matches the spec example `+92m`).
    assert_eq!(
        format_reset_countdown(now + Duration::minutes(5), now),
        "+5m"
    );
    assert_eq!(
        format_reset_countdown(now + Duration::minutes(92), now),
        "+92m"
    );
    // Hours from two hours up to two days.
    assert_eq!(format_reset_countdown(now + Duration::hours(5), now), "+5h");
    // Days beyond two days.
    assert_eq!(format_reset_countdown(now + Duration::days(3), now), "+3d");
    assert_eq!(format_reset_countdown(now + Duration::days(7), now), "+7d");
    // A reset that already elapsed clamps to +0m, never negative.
    assert_eq!(
        format_reset_countdown(now - Duration::minutes(5), now),
        "+0m"
    );
}

/// `free_in=` answers "when does it come back" — the question
/// `status=throttled` cannot. It lives on THIS surface because the TUI's
/// Status column is Length(9) and "throttled" is exactly 9 characters, so a
/// countdown there is clipped at every terminal width.
///
/// Asserts the RENDERED LINE and the emitted JSON, never a formatting helper.
/// A unit test on the helper is what let a clipped countdown ship green.
#[tokio::test]
async fn free_in_names_the_instant_and_is_omitted_when_none_can_be_promised() {
    // WindowProber gates alice on 5h (~+92m) and bob on 7d (+3d) — real gates
    // with known resets, so account_gate yields a free_at for both.
    let config = load_from(TWO_ACCOUNTS);
    let thresholds = config.switch_thresholds();
    let gated = snapshot_offline(config, Arc::new(NoRefresh), Arc::new(WindowProber), true).await;
    let text = render_accounts(&gated, StatusSource::Live);
    for line in text.lines() {
        assert!(
            line.contains("free_in="),
            "a gated account names its instant: {line}"
        );
        let secs: i64 = line
            .split("free_in=")
            .nth(1)
            .and_then(|t| t.split('s').next())
            .and_then(|n| n.parse().ok())
            .expect("free_in carries whole seconds");
        assert!(secs > 0, "a promised instant is in the future: {line}");
    }
    // JSON mirrors it for machine consumers, in ms like every other instant.
    let json = status_json(&gated, &thresholds, StatusSource::Live);
    assert!(
        json.contains("\"freeAtMs\""),
        "json carries freeAtMs: {json}"
    );
    assert!(
        json.contains("\"secondsUntilFree\""),
        "and the countdown: {json}"
    );

    // UNGATED: nothing binds, so there is no instant to promise. The token is
    // OMITTED, never rendered as zero — `free_in=0s` would read as "returns
    // now", which is the same false promise `free_at = None` exists to avoid.
    let config = load_from(TWO_ACCOUNTS);
    let open = snapshot_offline(
        config,
        Arc::new(NoRefresh),
        Arc::new(FixedProber { util: 0.25 }),
        true,
    )
    .await;
    let text = render_accounts(&open, StatusSource::Live);
    assert!(
        !text.contains("free_in="),
        "an account in rotation promises nothing: {text}"
    );
}

#[test]
fn reset_clock_is_bare_hh_mm_utc() {
    // 18:50:00 UTC → "18:50Z" regardless of seconds.
    let reset =
        OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap() + time::Duration::seconds(0);
    let clock = format_reset_clock(reset);
    assert!(clock.ends_with('Z'), "explicit UTC marker: {clock}");
    assert_eq!(clock.len(), 6, "HH:MMZ shape: {clock}");
    assert!(clock.contains(':'), "clock shape: {clock}");
}

/// A prober that drives one account over its 5h threshold and another over its
/// 7d threshold, each with a future reset — so both windows read as held.
struct WindowProber;
impl UsageProber for WindowProber {
    fn probe(&self, credential: crate::probe::ProbeCredential) -> ProbeFuture {
        let now = crate::now_ms();
        Box::pin(async move {
            if credential.access_token == "at-a" {
                Ok(Usage {
                    five_hour: crate::probe::WindowReading::Read(UsageBucket {
                        utilization: Some(0.95),
                        reset_at_ms: Some(now + 92 * 60 * 1000),
                    }),
                    seven_day: None,
                    seven_day_oi: None,
                    extra_usage_usd: None,
                    codex: None,
                    claude_resets: None,
                })
            } else {
                Ok(Usage {
                    five_hour: crate::probe::WindowReading::Unread,
                    seven_day: Some(UsageBucket {
                        utilization: Some(0.97),
                        reset_at_ms: Some(now + 3 * 24 * 60 * 60 * 1000),
                    }),
                    seven_day_oi: None,
                    extra_usage_usd: None,
                    codex: None,
                    claude_resets: None,
                })
            }
        })
    }
}

#[tokio::test]
async fn render_accounts_shows_held_window_and_reset_for_over_threshold_accounts() {
    // 3a: a held account's binding window carries an inline `(+countdown)`,
    // gated on the same per-account threshold `eligible` uses.
    let config = load_from(TWO_ACCOUNTS);
    let thresholds = config.switch_thresholds();
    let snapshot =
        snapshot_offline(config, Arc::new(NoRefresh), Arc::new(WindowProber), true).await;
    let text = render_accounts(&snapshot, StatusSource::Live);
    let alice = text
        .lines()
        .find(|l| l.contains("alice@example.com"))
        .expect("alice line");
    let bob = text
        .lines()
        .find(|l| l.contains("bob@example.com"))
        .expect("bob line");
    // alice (at-a) is 5h-held at 0.95 with a ~+92m reset; her 5h window carries
    // the inline countdown while her (unlearned) weekly reads n/a. The exact
    // minute drifts with wall-clock between probe and render, so match the `(+`.
    assert!(alice.contains("5h=95%(+"), "5h hold + countdown: {alice}");
    assert!(alice.contains("wk=n/a"), "alice weekly unknown: {alice}");
    // bob (at-b) is 7d-held at 0.97 with a +3d reset; his weekly carries the
    // countdown while his (unlearned) 5h reads n/a.
    assert!(bob.contains("wk=97%(+"), "wk hold + countdown: {bob}");
    assert!(bob.contains("5h=n/a"), "bob 5h unknown: {bob}");
    // Neither is over its 1.0 exhaustion line, so both read `near`, not `spent`.
    assert!(alice.contains("state=near"), "alice near-limit: {alice}");
    assert!(bob.contains("state=near"), "bob near-limit: {bob}");
    // The legacy suffix format is gone from the text line.
    assert!(!text.contains("held="), "no legacy held= suffix: {text}");
    // Greppable one-line-per-account contract survives the new fields.
    assert_eq!(
        text.lines().count(),
        2,
        "still one line per account: {text}"
    );

    // JSON mirrors the held fields for machine consumers.
    let json = status_json(&snapshot, &thresholds, StatusSource::Offline);
    assert!(json.contains("\"held\""), "json carries held array: {json}");
    assert!(
        json.contains("\"window\": \"5h\""),
        "json names the 5h window: {json}"
    );
    assert!(
        json.contains("\"window\": \"7d\""),
        "json names the 7d window: {json}"
    );
    assert!(
        json.contains("minutesUntilReset"),
        "json carries the countdown: {json}"
    );
}

/// A prober that puts alice's 5h window comfortably BELOW her threshold (a
/// live reset that was previously invisible on the wire), and leaves bob's
/// 5h window entirely unlearned (no live reset at all — the other
/// invisible case).
struct BelowThresholdWindowProber;
impl UsageProber for BelowThresholdWindowProber {
    fn probe(&self, credential: crate::probe::ProbeCredential) -> ProbeFuture {
        let now = crate::now_ms();
        Box::pin(async move {
            if credential.access_token == "at-a" {
                Ok(Usage {
                    five_hour: crate::probe::WindowReading::Read(UsageBucket {
                        utilization: Some(0.3),
                        reset_at_ms: Some(now + 42 * 60 * 1000),
                    }),
                    seven_day: None,
                    seven_day_oi: None,
                    extra_usage_usd: None,
                    codex: None,
                    claude_resets: None,
                })
            } else {
                Ok(Usage {
                    five_hour: crate::probe::WindowReading::Unread,
                    seven_day: None,
                    seven_day_oi: None,
                    extra_usage_usd: None,
                    codex: None,
                    claude_resets: None,
                })
            }
        })
    }
}

#[tokio::test]
async fn render_accounts_json_carries_reset_for_a_below_threshold_window() {
    // This is the exact case the fix restores: a window well under its
    // threshold (so it never appears in `held[]`) still has a live reset,
    // and that reset must now reach the JSON wire unconditionally.
    let config = load_from(TWO_ACCOUNTS);
    let thresholds = config.switch_thresholds();
    let snapshot = snapshot_offline(
        config,
        Arc::new(NoRefresh),
        Arc::new(BelowThresholdWindowProber),
        true,
    )
    .await;

    let json = status_json(&snapshot, &thresholds, StatusSource::Offline);
    let alice_row = status_row(&json, "alice@example.com");
    // 0.3 is far under the 0.9 default threshold, so alice's 5h window is
    // never a binding hold, yet its reset must still reach the wire.
    assert!(
        alice_row["fiveHourResetAtMs"].is_i64(),
        "below-threshold window's reset reaches the wire: {alice_row}"
    );
    // The below-threshold window never binds, so it is absent from `held[]`
    // even though its reset is now unconditionally on the wire.
    assert!(
        alice_row["held"]
            .as_array()
            .expect("held is an array")
            .is_empty(),
        "held[] stays empty for a non-binding window: {alice_row}"
    );
}

/// `tcr status --json` carries `quotaUpdatedAtMs`, the time of the newest
/// quota reading, on both the offline path (its own probe) and the live
/// path (the server's payload, rebuilt by `into_snapshot`), and the two
/// agree.
#[tokio::test]
async fn render_accounts_json_carries_quota_updated_at() {
    let config = load_from(TWO_ACCOUNTS);
    let thresholds = config.switch_thresholds();
    let before = crate::now_ms();
    let snapshot = snapshot_offline(
        config,
        Arc::new(NoRefresh),
        Arc::new(BelowThresholdWindowProber),
        true,
    )
    .await;
    let render = |snapshot: &StatsSnapshot, source: StatusSource| {
        let json = status_json(snapshot, &thresholds, source);
        status_row(&json, "alice@example.com")
    };

    let offline = render(&snapshot, StatusSource::Offline);
    let at = offline["quotaUpdatedAtMs"]
        .as_i64()
        .unwrap_or_else(|| panic!("a probed row carries quotaUpdatedAtMs: {offline}"));
    assert!(
        at >= before && at <= crate::now_ms(),
        "a fresh probe reads as recent: {at}"
    );

    let payload =
        StatusPayload::from_snapshot(&snapshot, &thresholds, false, None, Default::default());
    let wire = serde_json::to_string(&payload).expect("serialize");
    let back: StatusPayload = serde_json::from_str(&wire).expect("deserialize");
    let (rebuilt, rebuilt_thresholds) = back.into_snapshot();
    let live_json = status_json(&rebuilt, &rebuilt_thresholds, StatusSource::Live);
    let live: Vec<serde_json::Value> = serde_json::from_str(&live_json).expect("json");
    let live = live
        .iter()
        .find(|r| r["name"] == "alice@example.com")
        .expect("alice row");
    assert_eq!(
        live["quotaUpdatedAtMs"], offline["quotaUpdatedAtMs"],
        "the live path carries the same instant the server read"
    );
}

#[tokio::test]
async fn render_accounts_json_omits_reset_when_no_live_reset_exists() {
    let config = load_from(TWO_ACCOUNTS);
    let thresholds = config.switch_thresholds();
    let snapshot = snapshot_offline(
        config,
        Arc::new(NoRefresh),
        Arc::new(BelowThresholdWindowProber),
        true,
    )
    .await;

    let json = status_json(&snapshot, &thresholds, StatusSource::Offline);
    let bob_row = status_row(&json, "bob@example.com");
    assert_eq!(
        bob_row["fiveHourResetAtMs"],
        serde_json::Value::Null,
        "no live reset renders as null, never 0 or a fabricated instant: {bob_row}"
    );
    assert_eq!(
        bob_row["sevenDayResetAtMs"],
        serde_json::Value::Null,
        "no live reset renders as null, never 0 or a fabricated instant: {bob_row}"
    );
}

#[tokio::test]
async fn render_accounts_json_held_array_is_unchanged_for_over_threshold_account() {
    // The additive claim, asserted rather than assumed: adding the two new
    // reset fields must not change a single byte of the existing `held[]`
    // shape for an account whose window WAS already binding.
    let config = load_from(TWO_ACCOUNTS);
    let thresholds = config.switch_thresholds();
    let snapshot =
        snapshot_offline(config, Arc::new(NoRefresh), Arc::new(WindowProber), true).await;
    let json = status_json(&snapshot, &thresholds, StatusSource::Offline);
    let alice_row = status_row(&json, "alice@example.com");
    let held = alice_row["held"].as_array().expect("held is an array");
    assert_eq!(
        held.len(),
        1,
        "alice's 5h window is the sole hold: {held:?}"
    );
    let entry = &held[0];
    assert_eq!(entry["window"], "5h");
    assert!(
        entry["resetAtMs"].is_i64(),
        "held entry keeps resetAtMs: {entry}"
    );
    assert!(
        entry["minutesUntilReset"].is_i64(),
        "held entry keeps minutesUntilReset: {entry}"
    );
    // held[]'s own two keys are unaffected by the new top-level fields.
    assert_eq!(
        entry.as_object().expect("held entry is an object").len(),
        3,
        "held entry shape (window/resetAtMs/minutesUntilReset) is unchanged: {entry}"
    );
}

#[tokio::test]
async fn render_accounts_text_carries_countdown_for_a_below_threshold_window() {
    // The text-line half of the same restored invariant: a window well
    // under its threshold still has a live reset, so the greppable status
    // line must carry its `(+countdown)` even though the window never
    // binds a hold.
    let config = load_from(TWO_ACCOUNTS);
    let snapshot = snapshot_offline(
        config,
        Arc::new(NoRefresh),
        Arc::new(BelowThresholdWindowProber),
        true,
    )
    .await;
    let text = render_accounts(&snapshot, StatusSource::Live);
    let alice = text
        .lines()
        .find(|l| l.contains("alice@example.com"))
        .expect("alice line");
    assert!(
        alice.contains("5h=30%(+"),
        "below-threshold window still carries a countdown: {alice}"
    );
}

#[tokio::test]
async fn render_accounts_text_omits_countdown_when_no_live_reset_exists() {
    let config = load_from(TWO_ACCOUNTS);
    let snapshot = snapshot_offline(
        config,
        Arc::new(NoRefresh),
        Arc::new(BelowThresholdWindowProber),
        true,
    )
    .await;
    let text = render_accounts(&snapshot, StatusSource::Live);
    let bob = text
        .lines()
        .find(|l| l.contains("bob@example.com"))
        .expect("bob line");
    // bob's 5h window was never learned at all: n/a, no `(+…)` token.
    assert!(bob.contains("5h=n/a"), "unlearned window reads n/a: {bob}");
    assert!(
        !bob.contains("5h=n/a("),
        "no countdown token on an unlearned window: {bob}"
    );
}

/// A prober that drives every account's 5h window fully over the 1.0
/// exhaustion line, with a future reset — so the window binds and reads `spent`.
struct ExhaustedProber;
impl UsageProber for ExhaustedProber {
    fn probe(&self, _credential: crate::probe::ProbeCredential) -> ProbeFuture {
        let now = crate::now_ms();
        Box::pin(async move {
            Ok(Usage {
                five_hour: crate::probe::WindowReading::Read(UsageBucket {
                    utilization: Some(1.0),
                    reset_at_ms: Some(now + 45 * 60 * 1000),
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

#[tokio::test]
async fn render_accounts_marks_fully_exhausted_account_as_spent_with_countdown() {
    let config = load_from(TWO_ACCOUNTS);
    let snapshot =
        snapshot_offline(config, Arc::new(NoRefresh), Arc::new(ExhaustedProber), true).await;
    let text = render_accounts(&snapshot, StatusSource::Live);
    let line = text
        .lines()
        .find(|l| l.contains("alice@example.com"))
        .expect("alice line");
    // At 1.0 util the account is truly spent (never masquerading as `full`),
    // and its binding 5h window carries an inline `(+…)` countdown. The exact
    // minute drifts with wall-clock between probe and render, so match the `(+`.
    assert!(line.contains("state=spent"), "exhausted → spent: {line}");
    assert!(
        line.contains("5h=100%(+"),
        "5h exhausted + countdown: {line}"
    );
}

// ---------------------------------------------------------------------
// The golden `tcr status --json` contract fixture.
// ---------------------------------------------------------------------
