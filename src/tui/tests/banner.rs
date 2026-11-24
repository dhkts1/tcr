//! Tests of the fleet banner, its Codex route reasons, and the facts it is drawn from.

use super::*;

#[test]
fn fleet_status_counts_eligible_and_skips_next_free() {
    let now = anchor();
    let accounts = vec![
        snap_gate("a", GateReason::Ok, None),
        snap_gate(
            "b",
            GateReason::FiveHour,
            Some(now + TimeDuration::seconds(300)),
        ),
        snap_gate("c", GateReason::Ok, None),
    ];
    let status = fleet_status(&accounts, now);
    assert_eq!((status.eligible, status.total), (2, 3));
    // Some account is in rotation, so no "next free" is computed.
    assert!(status.next_free.is_none());
}

#[test]
fn fleet_status_names_soonest_recovery_when_none_eligible() {
    let now = anchor();
    // All gated; b returns first (300s) even though it is listed second, and the
    // never-self-freeing Login account is skipped.
    let accounts = vec![
        snap_gate(
            "a",
            GateReason::SevenDay,
            Some(now + TimeDuration::seconds(5_000)),
        ),
        snap_gate(
            "b",
            GateReason::FiveHour,
            Some(now + TimeDuration::seconds(300)),
        ),
        snap_gate("login", GateReason::Login, None),
    ];
    let status = fleet_status(&accounts, now);
    assert_eq!(status.eligible, 0);
    let (name, delta) = status.next_free.expect("some account has a known free_at");
    assert_eq!(name, "b");
    assert_eq!(delta, TimeDuration::seconds(300));
}

#[test]
fn fleet_status_unknown_when_all_gated_without_reset() {
    let now = anchor();
    // Every account is out with NO known clear-instant → next_free is None, so
    // the banner honestly says "unknown" rather than promising a time.
    let accounts = vec![
        snap_gate("a", GateReason::Login, None),
        snap_gate("b", GateReason::Disabled, None),
    ];
    let status = fleet_status(&accounts, now);
    assert_eq!(status.eligible, 0);
    assert!(status.next_free.is_none());
}

/// What the dashboard asks the process each tick is gated on its fleet, as every Codex
/// surface is: a Claude-only fleet asks nothing about codex at all, an empty one asks only
/// whether a codex login is on disk, and a fleet holding Codex rows asks for the route with
/// their count and never for a login.
#[test]
fn dashboard_facts_are_gated_on_the_fleet() {
    let home = Some(PathBuf::from("/Users/example"));
    let claude_only = snapshot_of(vec![snap_gate("alice@example.com", GateReason::Ok, None)]);
    let facts = dashboard_facts(
        &claude_only,
        home.clone(),
        |_| panic!("a Claude-only fleet asks for no Codex route"),
        || panic!("a Claude-only fleet asks for no Codex traffic"),
        || panic!("a fleet with accounts checks for no codex login"),
    );
    assert_eq!(facts.codex_route, None);
    assert_eq!(facts.ca_path, None);
    assert_eq!(facts.codex_traffic, None);
    assert_eq!(facts.codex_login, None);
    assert_eq!(facts.home, home);

    let login = PathBuf::from("/Users/example/.codex/auth.json");
    let facts = dashboard_facts(
        &snapshot_of(vec![]),
        home.clone(),
        |_| panic!("an empty fleet asks for no Codex route"),
        || panic!("an empty fleet asks for no Codex traffic"),
        || Some(login.clone()),
    );
    assert_eq!(facts.codex_login, Some(login));

    let asked = std::cell::Cell::new(None);
    let ca = PathBuf::from("/Users/example/.config/tcr-ca.pem");
    let facts = dashboard_facts(
        &snapshot_of(vec![
            snap_gate("alice@example.com", GateReason::Ok, None),
            codex_gate("bob@example.com/codex", GateReason::Ok, None),
        ]),
        home,
        |codex_rows| {
            asked.set(Some(codex_rows));
            (
                Some(CodexRoute {
                    pooled: true,
                    ..Default::default()
                }),
                Some(ca.clone()),
            )
        },
        || Some(CodexTraffic::default()),
        || panic!("a fleet with accounts checks for no codex login"),
    );
    assert_eq!(asked.get(), Some(1), "asked with its one Codex row");
    assert!(facts.codex_route.is_some_and(|route| route.pooled));
    assert_eq!(facts.ca_path, Some(ca));
    assert_eq!(facts.codex_traffic, Some(CodexTraffic::default()));
    assert_eq!(facts.codex_login, None);
}

/// The text of `line`, its spans joined.
fn line_text(line: &Line) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

/// The style of the one span of `line` whose text is `text`, or a panic naming the spans.
fn span_style(line: &Line, text: &str) -> Style {
    let Some(span) = line.spans.iter().find(|span| span.content == text) else {
        panic!("no span reads {text:?}: {:?}", line.spans);
    };
    span.style
}

fn green_bold() -> Style {
    Style::default()
        .fg(Color::Green)
        .add_modifier(Modifier::BOLD)
}

fn dim() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

/// The banner gives each provider a segment of its own, counted over its own accounts and
/// naming its own next free account, so it says what each client's 429 says: a spent Claude
/// fleet is never shown green because a Codex account is ready. Each segment takes its own
/// style; a fleet of one provider keeps one segment.
#[test]
fn fleet_banner_splits_by_provider() {
    let now = anchor();
    let at = |secs: i64| Some(now + TimeDuration::seconds(secs));
    let banner = |accounts: Vec<AccountSnapshot>| {
        fleet_banner_line(&snapshot_of(accounts), &DashboardFacts::default(), now, 174)
    };

    let claude_out = banner(vec![
        snap_gate(
            "alice@example.com",
            GateReason::FiveHour,
            at(2 * 3_600 + 59 * 60 + 30),
        ),
        snap_gate(
            "frank@example.com",
            GateReason::SevenDay,
            at(3 * 86_400 + 30),
        ),
        snap_gate("erin@example.com", GateReason::Login, None),
        codex_gate("bob@example.com/codex", GateReason::Ok, None),
        codex_gate("dana@example.com", GateReason::Ok, None),
    ]);
    assert_eq!(
        line_text(&claude_out),
        "CLAUDE 0/3 eligible · next free alice@example.com in 2h59m │ CODEX 2/2 eligible"
    );
    assert_eq!(
        span_style(
            &claude_out,
            "CLAUDE 0/3 eligible · next free alice@example.com in 2h59m"
        ),
        alarm_style()
    );
    assert_eq!(span_style(&claude_out, " │ "), dim());
    assert_eq!(span_style(&claude_out, "CODEX 2/2 eligible"), green_bold());

    let codex_out = banner(vec![
        snap_gate("alice@example.com", GateReason::Ok, None),
        snap_gate("frank@example.com", GateReason::Ok, None),
        codex_gate(
            "bob@example.com/codex",
            GateReason::SevenDay,
            at(4 * 86_400 + 30),
        ),
        codex_gate(
            "carol@example.com",
            GateReason::SevenDay,
            at(3 * 86_400 + 30),
        ),
    ]);
    assert_eq!(
        line_text(&codex_out),
        "CLAUDE 2/2 eligible │ CODEX 0/2 eligible · next free carol@example.com in 3d"
    );
    assert_eq!(span_style(&codex_out, "CLAUDE 2/2 eligible"), green_bold());
    assert_eq!(
        span_style(
            &codex_out,
            "CODEX 0/2 eligible · next free carol@example.com in 3d"
        ),
        alarm_style()
    );

    let codex_only = banner(vec![
        codex_gate("bob@example.com/codex", GateReason::Ok, None),
        codex_gate("dana@example.com", GateReason::Ok, None),
        codex_gate(
            "carol@example.com",
            GateReason::SevenDay,
            at(3 * 86_400 + 30),
        ),
        codex_gate("erin@example.com", GateReason::Login, None),
    ]);
    assert_eq!(line_text(&codex_only), "CODEX 2/4 eligible");
    assert_eq!(span_style(&codex_only, "CODEX 2/4 eligible"), green_bold());

    let both_out = banner(vec![
        snap_gate(
            "alice@example.com",
            GateReason::FiveHour,
            at(2 * 3_600 + 59 * 60 + 30),
        ),
        codex_gate(
            "bob@example.com/codex",
            GateReason::FiveHour,
            at(5 * 60 + 30),
        ),
    ]);
    assert_eq!(
        line_text(&both_out),
        "CLAUDE 0/1 eligible · next free alice@example.com in 2h59m │ \
         CODEX 0/1 eligible · next free bob@example.com/codex in 5m",
        "each tail names its own provider's account"
    );

    let claude_only = banner(vec![
        snap_gate("alice@example.com", GateReason::FiveHour, at(47 * 60 + 30)),
        snap_gate("frank@example.com", GateReason::Login, None),
    ]);
    assert_eq!(
        claude_only.spans,
        vec![Span::styled(
            "FLEET 0/2 eligible · next free alice@example.com in 47m",
            alarm_style()
        )],
        "a Claude-only fleet keeps its one FLEET segment"
    );
}

/// A fleet of two Claude accounts out and a Codex pool whose two accounts are both spent,
/// carol first: the fleet the fitting tests shorten.
fn spent_codex_pool() -> StatsSnapshot {
    let now = anchor();
    let at = |secs: i64| Some(now + TimeDuration::seconds(secs));
    snapshot_of(vec![
        snap_gate("alice@example.com", GateReason::Ok, None),
        snap_gate("frank@example.com", GateReason::FiveHour, at(3_600 + 30)),
        snap_gate("erin@example.com", GateReason::Login, None),
        codex_gate(
            "bob@example.com/codex",
            GateReason::SevenDay,
            at(4 * 86_400 + 30),
        ),
        codex_gate(
            "carol@example.com",
            GateReason::SevenDay,
            at(3 * 86_400 + 30),
        ),
    ])
}

/// A route that does not pool codex, with `code`.
fn not_pooled(code: &str) -> CodexRoute {
    CodexRoute {
        pooled: false,
        code: Some(code.to_string()),
        reason: Some(format!("the launcher's sentence for {code}")),
        upstream: None,
    }
}

/// When codex is not pooled the CODEX segment says why, in a few words keyed on the route's
/// code: the same decision `tcr status` words in full.
#[test]
fn not_pooled_suffix_uses_codex_route() {
    let snapshot = snapshot_of(vec![codex_gate(
        "bob@example.com/codex",
        GateReason::Ok,
        None,
    )]);
    let yellow = Style::default().fg(Color::Yellow);
    for (code, suffix) in [
        (
            "mitm-off",
            " · not pooled: MITM off (TLS material failed to load)",
        ),
        (
            "leaf-missing-host",
            " · not pooled: leaf does not cover chatgpt.com",
        ),
        (
            "no-ca",
            " · not pooled: no readable CA at /Users/example/.config/tcr-ca.pem",
        ),
        ("unknown-leaf", " · not pooled: leaf state unknown"),
    ] {
        let facts = DashboardFacts {
            codex_route: Some(not_pooled(code)),
            ca_path: Some(PathBuf::from("/Users/example/.config/tcr-ca.pem")),
            ..DashboardFacts::default()
        };
        let line = fleet_banner_line(&snapshot, &facts, anchor(), 174);
        assert_eq!(
            line_text(&line),
            format!("CODEX 1/1 eligible{suffix}"),
            "{code}"
        );
        assert_eq!(span_style(&line, suffix), yellow, "{code}");
    }

    let pooled = DashboardFacts {
        codex_route: Some(CodexRoute {
            pooled: true,
            ..Default::default()
        }),
        ..DashboardFacts::default()
    };
    assert_eq!(
        line_text(&fleet_banner_line(&snapshot, &pooled, anchor(), 174)),
        "CODEX 1/1 eligible"
    );
}

/// A banner that does not fit gives up its least urgent words first: the unpooled count,
/// then the refused count, then the names in the next-free tails, then the tails. The
/// counts themselves always stay.
#[test]
fn the_banner_drops_unpooled_then_refused_then_shortens_next_free() {
    let snapshot = spent_codex_pool();
    let facts = DashboardFacts {
        codex_traffic: Some(CodexTraffic {
            refused_10m: 12,
            unpooled_10m: 3,
            ..Default::default()
        }),
        ..DashboardFacts::default()
    };
    let line = |width: u16| fleet_banner_line(&snapshot, &facts, anchor(), width);

    let full = line(116);
    assert_eq!(
        line_text(&full),
        "CLAUDE 1/3 eligible │ CODEX 0/2 eligible · next free carol@example.com in 3d \
         · 12 refused in 10m · 3 unpooled in 10m"
    );
    assert_eq!(
        span_style(&full, " · 12 refused in 10m"),
        Style::default().fg(Color::Yellow)
    );
    assert_eq!(span_style(&full, " · 3 unpooled in 10m"), dim());
    assert_eq!(
        line_text(&line(110)),
        "CLAUDE 1/3 eligible │ CODEX 0/2 eligible · next free carol@example.com in 3d \
         · 12 refused in 10m"
    );
    assert_eq!(
        line_text(&line(90)),
        "CLAUDE 1/3 eligible │ CODEX 0/2 eligible · next free carol@example.com in 3d"
    );
    assert_eq!(
        line_text(&line(70)),
        "CLAUDE 1/3 eligible │ CODEX 0/2 eligible · next free in 3d"
    );
}

/// A not-pooled reason is never dropped: when nothing else is left to give, the reason is
/// cut, visibly, and ` · not pooled: ` stays whole.
#[test]
fn not_pooled_reason_is_truncated_visibly() {
    let facts = DashboardFacts {
        codex_route: Some(not_pooled("no-ca")),
        ca_path: Some(PathBuf::from("/Users/example/.config/tcr-ca.pem")),
        ..DashboardFacts::default()
    };
    let line = fleet_banner_line(&spent_codex_pool(), &facts, anchor(), 100);
    let text = line_text(&line);
    assert_eq!(
        text,
        "CLAUDE 1/3 eligible │ CODEX 0/2 eligible · not pooled: no readable CA at \
         /Users/example/.config/tcr…"
    );
    assert_eq!(text.chars().count(), 100);
    assert_eq!(
        span_style(
            &line,
            " · not pooled: no readable CA at /Users/example/.config/tcr…"
        ),
        Style::default().fg(Color::Yellow)
    );
}

/// An empty fleet on a Mac where the codex CLI has a login of its own says how to add a
/// Codex account, and where that login is, by its existence alone. Its banner is not the
/// alarm, since nothing is down: there is nothing yet. Without such a login, the empty
/// fleet draws as it always did.
#[test]
fn empty_fleet_with_codex_login_drops_next_free() {
    let empty = snapshot_of(vec![]);
    let facts = DashboardFacts {
        codex_login: Some(PathBuf::from("/Users/example/.codex/auth.json")),
        home: Some(PathBuf::from("/Users/example")),
        ..DashboardFacts::default()
    };
    let banner = fleet_banner_line(&empty, &facts, anchor(), 110);
    assert_eq!(
        banner.spans,
        vec![Span::styled("FLEET 0/0 eligible", dim())]
    );

    let draw = |facts: &DashboardFacts| {
        let mut terminal =
            Terminal::new(TestBackend::new(110, 12)).expect("test backend builds a terminal");
        terminal
            .draw(|frame| render_with(frame, &empty, 0, None, facts, anchor()))
            .expect("render succeeds");
        terminal.backend().buffer().clone()
    };
    let buffer = draw(&facts);
    let rows = buffer_rows(&buffer);
    let (y, row) = row_holding(
        &rows,
        "no accounts — tcr login (Claude) · tcr login --codex (Codex)",
    );
    let x = cell_x(row, "no accounts");
    assert_eq!(buffer[(x, y as u16)].modifier, Modifier::empty(), "{row}");
    let (y, row) = row_holding(
        &rows,
        "found a codex login at ~/.codex/auth.json — tcr login --codex gives tcr a login of its own",
    );
    let x = cell_x(row, "found");
    assert!(
        buffer[(x, y as u16)].modifier.contains(Modifier::DIM),
        "{row}"
    );

    let without = draw(&DashboardFacts::default());
    let mut terminal =
        Terminal::new(TestBackend::new(110, 12)).expect("test backend builds a terminal");
    terminal
        .draw(|frame| render(frame, &empty, 0, None))
        .expect("render succeeds");
    assert_eq!(
        &without,
        terminal.backend().buffer(),
        "with no codex login, the empty fleet draws as it always did"
    );
}

/// The alarm styling is defined once. Two literals would drift, and a notice
/// that stopped looking like the fleet-down banner would read as ordinary
/// chrome — which is exactly what it is not.
#[test]
fn the_notice_and_the_downed_fleet_banner_share_one_alarm_style() {
    let style = alarm_style();
    assert_eq!(style.fg, Some(Color::Red));
    assert!(style.add_modifier.contains(Modifier::BOLD));
}
