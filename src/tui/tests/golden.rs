//! Whole-frame goldens: the demo fleet, a Claude-only fleet and an empty one.

use super::*;

/// What `tcr demo` draws on its first tick, cursor on the first row and no notice, at the
/// Full layout's width and at a Compact one: both providers, each in its own banner segment
/// and its own section with its own ▶, and Codex traffic in the sessions tree and the log.
/// `--nocapture` prints both frames.
#[test]
fn the_demo_draws_both_providers() {
    let manager = crate::demo::demo_manager();
    let facts = crate::demo::demo_facts();
    let now = OffsetDateTime::now_utc();
    let snapshot = manager.snapshot(now);
    for width in [FULL_LAYOUT_MIN_WIDTH, 110] {
        let mut terminal =
            Terminal::new(TestBackend::new(width, 40)).expect("test backend builds a terminal");
        terminal
            .draw(|frame| render_with(frame, &snapshot, 0, None, &facts, now))
            .expect("render succeeds");
        let rows = buffer_rows(terminal.backend().buffer());
        let frame = rows.join("\n");
        println!("== {width}x40\n{frame}");

        assert!(
            rows[0].starts_with("CLAUDE ") && rows[0].contains(" │ CODEX "),
            "{frame}"
        );
        assert!(
            rows.iter()
                .any(|row| row.starts_with("┌ tcr · accounts · claude (6) ")),
            "{frame}"
        );
        assert!(
            rows.iter().any(|row| row.starts_with("├ codex (3) ")),
            "{frame}"
        );
        let marked: Vec<&String> = rows.iter().filter(|row| row.starts_with("│▶ ")).collect();
        assert_eq!(marked.len(), 2, "one ▶ per provider\n{frame}");
        assert!(
            marked.iter().any(|row| row.contains("team-prod")),
            "{frame}"
        );
        assert!(
            marked.iter().any(|row| row.contains("bob@e…/codex")),
            "{frame}"
        );
        let (_, bob) = row_holding(&rows, "bob@e…/codex");
        assert!(bob.contains("none"), "{frame}");
        let (_, carol) = row_holding(&rows, "carol@examp…");
        assert!(carol.contains("7D 3d"), "{frame}");
        row_holding(&rows, "▾ bob@exa…/codex · 1 codex");
        row_holding(&rows, "…/codex/responses");
    }
}

/// A session sitting on its pin, last seen at `last_seen`. The golden times its rows from
/// the clock `render` reads, so [`snap`]'s seconds after the epoch do not fit it.
fn snap_at(id: &str, account: &str, requests: u64, last_seen: OffsetDateTime) -> SessionSnapshot {
    SessionSnapshot {
        last_seen: Some(last_seen),
        ..snap(id, account, requests, None)
    }
}

/// [`diverted`], last seen at `last_seen`.
fn diverted_at(
    id: &str,
    account: &str,
    served_by: &str,
    requests: u64,
    last_seen: OffsetDateTime,
) -> SessionSnapshot {
    SessionSnapshot {
        last_seen: Some(last_seen),
        ..diverted(id, account, served_by, requests, None)
    }
}

/// The Claude-only fleet the golden pins: eight accounts, one in each gate the dashboard
/// draws a chip for, three sessions (one of them diverted) and three log lines.
///
/// HOLD is left out on purpose: its chip counts raw seconds, so two runs a second apart
/// would draw two different frames. Every other time sits a whole unit plus 30 seconds from
/// `now`, so the few milliseconds between building this snapshot and `render` reading its own
/// clock cannot move a printed age or countdown.
fn claude_only_golden_snapshot(now: OffsetDateTime, alice_eligible: bool) -> StatsSnapshot {
    let after = |secs: i64| now + TimeDuration::seconds(secs);
    let before = |secs: i64| now - TimeDuration::seconds(secs);
    const MINUTE: i64 = 60;
    const HOUR: i64 = 3_600;
    const DAY: i64 = 86_400;

    let mut alice = if alice_eligible {
        snap_gate("alice@example.com", GateReason::Ok, None)
    } else {
        snap_gate(
            "alice@example.com",
            GateReason::FiveHour,
            Some(after(47 * MINUTE + 30)),
        )
    };
    alice.organization_type = Some("claude_max".to_string());
    alice.rate_limit_tier = Some("default_claude_max_20x".to_string());
    alice.five_hour = Some(0.47);
    alice.five_hour_reset = Some(after(2 * HOUR + 39 * MINUTE + 30));
    alice.seven_day = Some(0.20);
    alice.seven_day_reset = Some(after(4 * DAY + 30));
    alice.seven_day_oi = Some(0.62);
    alice.seven_day_oi_reset = Some(after(4 * DAY + 30));
    alice.requests = 1200;
    alice.input_tokens = 4_200_000;
    alice.output_tokens = 310_000;
    alice.cache_read_tokens = 3_900_000;
    alice.last_used = Some(before(3 * MINUTE + 30));
    alice.probe_status = ProbeStatus::Ok;
    alice.last_probe = Some(before(12 * MINUTE + 30));

    let mut frank = snap_gate(
        "frank@example.com",
        GateReason::SevenDay,
        Some(after(3 * DAY + 30)),
    );
    frank.organization_type = Some("claude_team".to_string());
    frank.rate_limit_tier = Some("default_claude_max_5x".to_string());
    frank.seat_tier = Some("team_tier_1".to_string());
    frank.five_hour = Some(0.10);
    frank.five_hour_reset = Some(after(4 * HOUR + 10 * MINUTE + 30));
    frank.seven_day = Some(0.96);
    frank.seven_day_reset = Some(after(3 * DAY + 30));
    frank.quota_state = QuotaState::NearLimit;
    frank.probe_status = ProbeStatus::RateLimited;
    frank.last_probe = Some(before(2 * MINUTE + 30));
    frank.stream_error_count = 2;
    frank.requests = 2100;

    let mut erin = snap_gate("erin@example.com", GateReason::Login, None);
    erin.status = "error".to_string();
    erin.probe_status = ProbeStatus::Error;
    erin.last_probe = Some(before(45 * MINUTE + 30));

    let mut grace = snap_gate("grace@example.com", GateReason::Disabled, None);
    grace.organization_type = Some("claude_pro".to_string());
    grace.seven_day = Some(0.30);
    grace.seven_day_reset = Some(after(5 * DAY + 30));

    let mut henry = snap_gate("henry@example.com", GateReason::Reserved, None);
    henry.seven_day = Some(1.00);
    henry.seven_day_reset = Some(after(2 * DAY + 30));
    henry.quota_state = QuotaState::Exhausted;
    henry.groups = vec!["research".to_string()];
    henry.reserved_groups = vec!["research".to_string()];

    let mut ivy = snap_gate("ivy@example.com", GateReason::Rejected, None);
    ivy.five_hour = Some(0.88);
    ivy.five_hour_reset = Some(after(HOUR + 30));

    let mut jack = snap_gate(
        "jack@example.com",
        GateReason::FableWeekly,
        Some(after(2 * DAY + 30)),
    );
    jack.seven_day_oi = Some(1.00);
    jack.seven_day_oi_reset = Some(after(2 * DAY + 30));

    let mut kate = snap_gate("kate@example.com", GateReason::Parked, None);
    kate.groups = vec!["orchard".to_string()];
    kate.parked_groups = vec!["orchard".to_string()];

    StatsSnapshot {
        accounts: vec![alice, frank, erin, grace, henry, ivy, jack, kate],
        current: Some(0),
        current_by_provider: crate::stats::CurrentByProvider::default(),
        recent: vec![
            log_entry(
                200,
                "POST",
                "/v1/messages",
                "alice@example.com",
                before(MINUTE + 30),
            ),
            log_entry(
                429,
                "POST",
                "/v1/messages",
                "frank@example.com",
                before(2 * MINUTE + 30),
            ),
            log_entry(
                500,
                "GET",
                "/v1/messages/count_tokens?beta=true",
                "ivy@example.com",
                before(3 * MINUTE + 30),
            ),
        ],
        sessions: vec![
            snap_at("a1f3c2", "alice@example.com", 24, before(MINUTE + 30)),
            diverted_at(
                "b2c7d4",
                "alice@example.com",
                "frank@example.com",
                6,
                before(2 * MINUTE + 30),
            ),
            snap_at("c3d8e5", "frank@example.com", 12, before(5 * MINUTE + 30)),
        ],
        wire_sessions: vec![],
        wire_sessions_summary: tcr_status_wire::SessionsSummary::default(),
    }
}

/// A snapshot with no accounts, no sessions and no log: a fleet nobody has logged into.
fn empty_snapshot() -> StatsSnapshot {
    StatsSnapshot {
        accounts: vec![],
        current: None,
        current_by_provider: crate::stats::CurrentByProvider::default(),
        recent: vec![],
        sessions: vec![],
        wire_sessions: vec![],
        wire_sessions_summary: tcr_status_wire::SessionsSummary::default(),
    }
}

/// The switch that rewrites the golden below instead of comparing against it.
const UPDATE_TUI_GOLDEN: &str = "TCR_UPDATE_TUI_GOLDEN";

/// **The Claude-only guarantee for the dashboard.** Four frames of a fleet with no Codex
/// account, captured from the dashboard before it learned about Codex accounts: 174 columns
/// (the full layout) and 110 columns (the compact one) with one account eligible, 110
/// columns with nothing eligible, and an empty fleet. The frames are ratatui's `Buffer`
/// debug form, which prints every cell's text and every change of style, so a colour or a
/// modifier that moves fails this as surely as a character does.
///
/// Its four account titles follow main's project rename to `tcr`; every other cell and
/// style remains the original baseline. The golden is never regenerated to fit a change.
/// A Claude-only frame that differs from it
/// is a change a Claude-only user sees, and that is the bug.
#[test]
fn claude_only_frames_are_unchanged() {
    let mut text = String::new();
    let frames: [(&str, u16, Option<bool>); 4] = [
        (
            "174 columns, one account eligible",
            FULL_LAYOUT_MIN_WIDTH,
            Some(true),
        ),
        ("110 columns, one account eligible", 110, Some(true)),
        ("110 columns, nothing eligible", 110, Some(false)),
        ("110 columns, an empty fleet", 110, None),
    ];
    for (label, width, alice_eligible) in frames {
        let snapshot = match alice_eligible {
            Some(eligible) => claude_only_golden_snapshot(OffsetDateTime::now_utc(), eligible),
            None => empty_snapshot(),
        };
        let mut terminal =
            Terminal::new(TestBackend::new(width, 40)).expect("test backend builds a terminal");
        terminal
            .draw(|frame| render(frame, &snapshot, 2, None))
            .expect("render succeeds");
        text.push_str(&format!("== {label}\n{:?}\n", terminal.backend().buffer()));
    }

    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/tui-claude-only.golden");
    if std::env::var_os(UPDATE_TUI_GOLDEN).is_some() {
        std::fs::write(&path, &text)
            .unwrap_or_else(|err| panic!("cannot write {}: {err}", path.display()));
        return;
    }
    let golden = std::fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "cannot read the golden at {} ({err}); it is written once, by \
             {UPDATE_TUI_GOLDEN}=1, before the change it guards",
            path.display()
        )
    });
    assert_eq!(
        text, golden,
        "a Claude-only dashboard frame changed. The golden is never regenerated with \
         {UPDATE_TUI_GOLDEN} to fit a change: a Claude-only frame that differs from it is a \
         change every Claude-only user sees"
    );
}

/// Is `cell` painted in exactly `style`: its colour where `style` sets one, and exactly its
/// modifiers?
fn painted_with(cell: &ratatui::buffer::Cell, style: Style) -> bool {
    style.fg.is_none_or(|fg| cell.fg == fg)
        && style.bg.is_none_or(|bg| cell.bg == bg)
        && cell.modifier == style.add_modifier
}

/// An empty fleet with no codex login on disk is today's, word for word: the alarm banner
/// with no time to promise, and the empty accounts pane. Nothing on it may mention codex.
#[test]
fn empty_fleet_without_evidence_is_unchanged() {
    let snapshot = empty_snapshot();
    let mut terminal =
        Terminal::new(TestBackend::new(110, 12)).expect("test backend builds a terminal");
    terminal
        .draw(|frame| render(frame, &snapshot, 0, None))
        .expect("render succeeds");
    let buffer = terminal.backend().buffer();
    let rows = buffer_rows(buffer);
    let screen = rows.join("\n");

    assert_eq!(
        rows[0].trim_end(),
        "FLEET 0/0 eligible · next free unknown",
        "{screen}"
    );
    assert!(
        painted_with(&buffer[(0, 0)], alarm_style()),
        "the banner of an empty fleet is the alarm: {:?}",
        buffer[(0, 0)]
    );
    assert!(
        rows.iter().any(|row| row.contains(" tcr · accounts (0) ")),
        "{screen}"
    );
    assert!(
        !rows.iter().any(|row| row.to_lowercase().contains("codex")),
        "an empty fleet with no codex login says nothing about codex:\n{screen}"
    );
}
