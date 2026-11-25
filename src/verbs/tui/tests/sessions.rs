//! Tests of the session tree, the sessions pane and the request log.

use super::*;

#[test]
fn the_log_pane_advertises_the_redraw_key() {
    // The key is only an escape hatch if a user staring at a broken screen can
    // find it. If the title is re-worded, re-word this with it.
    let snapshot = util_snapshot(QuotaState::Normal);
    let mut terminal =
        Terminal::new(TestBackend::new(120, 6)).expect("test backend builds a terminal");
    terminal
        .draw(|frame| render_log(frame, frame.area(), &snapshot, anchor()))
        .expect("render succeeds");
    let painted = buffer_rows(terminal.backend().buffer()).join("\n");
    assert!(
        painted.contains("r redraw"),
        "redraw key missing from the log pane title:\n{painted}"
    );
}

/// A fallback (unpinned) session — no stable client identity, so it folds into
/// the single trailing aggregate row instead of the account tree.
fn unpinned(id: &str, account: &str, requests: u64, last_seen: Option<i64>) -> SessionSnapshot {
    SessionSnapshot {
        kind: SessionKind::Fallback,
        ..snap(id, account, requests, last_seen)
    }
}

/// The (name, count) of an account header row, or panic — keeps the ordering
/// assertions short and readable.
fn header(row: &TreeRow) -> (&str, usize) {
    match row {
        TreeRow::Account { name, count, .. } => (name.as_str(), *count),
        TreeRow::Session { id, .. } => panic!("expected an account header, got session {id}"),
        TreeRow::Unpinned { count, .. } => {
            panic!("expected an account header, got unpinned aggregate of {count}")
        }
    }
}

/// The id of a session row, or panic.
fn session_id(row: &TreeRow) -> &str {
    match row {
        TreeRow::Session { id, .. } => id.as_str(),
        TreeRow::Account { name, .. } => panic!("expected a session, got header {name}"),
        TreeRow::Unpinned { count, .. } => {
            panic!("expected a session, got unpinned aggregate of {count}")
        }
    }
}

#[test]
fn session_tree_preserves_recency_order_across_and_within_groups() {
    // Most-recent-first input: acct-a, then acct-b, then acct-a again.
    let sessions = vec![
        snap("a1", "acct-a", 1, Some(100)),
        snap("b1", "acct-b", 1, Some(90)),
        snap("a2", "acct-a", 1, Some(80)),
    ];
    let rows = session_tree(&sessions);

    // acct-a appeared first, so its group leads with both sessions in input
    // order, then acct-b's group.
    assert_eq!(rows.len(), 5);
    assert_eq!(header(&rows[0]), ("acct-a", 2));
    assert_eq!(session_id(&rows[1]), "a1");
    assert_eq!(session_id(&rows[2]), "a2");
    assert_eq!(header(&rows[3]), ("acct-b", 1));
    assert_eq!(session_id(&rows[4]), "b1");
}

#[test]
fn session_tree_account_row_sums_requests_and_takes_youngest_last_seen() {
    let sessions = vec![
        snap("a1", "acct-a", 12, Some(50)),
        snap("a2", "acct-a", 8, Some(200)), // youngest of the group
        snap("a3", "acct-a", 5, None),      // never seen — must not win
    ];
    let rows = session_tree(&sessions);

    match &rows[0] {
        TreeRow::Account {
            count,
            requests,
            last_seen,
            ..
        } => {
            assert_eq!(*count, 3);
            assert_eq!(*requests, 25);
            assert_eq!(*last_seen, Some(at(200)));
        }
        _ => panic!("row 0 should be the account header"),
    }
}

#[test]
fn session_tree_single_account_yields_one_header_and_all_children() {
    let sessions = vec![
        snap("s1", "solo", 1, Some(10)),
        snap("s2", "solo", 1, Some(9)),
        snap("s3", "solo", 1, Some(8)),
    ];
    let rows = session_tree(&sessions);

    assert_eq!(rows.len(), 4); // one header + three sessions
    assert_eq!(header(&rows[0]), ("solo", 3));
    for row in &rows[1..] {
        assert!(
            matches!(row, TreeRow::Session { .. }),
            "children must be sessions"
        );
    }
}

#[test]
fn session_tree_empty_input_is_empty() {
    assert!(session_tree(&[]).is_empty());
}

#[test]
fn session_tree_groups_a_diverted_session_under_its_pin() {
    // `d1` is pinned to acct-a; its last request was diverted to acct-b. It
    // must stay in acct-a's group — the pin never moved — and carry the
    // divert as an annotation rather than a change of home.
    let sessions = vec![
        snap("a1", "acct-a", 4, Some(100)),
        diverted("d1", "acct-a", "acct-b", 7, Some(90)),
    ];
    let rows = session_tree(&sessions);

    assert_eq!(rows.len(), 3, "one header, two sessions — NO acct-b group");
    assert_eq!(header(&rows[0]), ("acct-a", 2));
    match (&rows[1], &rows[2]) {
        (
            TreeRow::Session {
                diverted_to: home, ..
            },
            TreeRow::Session {
                id,
                diverted_to: away,
                ..
            },
        ) => {
            assert_eq!(id, "d1");
            assert_eq!(*home, None, "a session sitting on its pin is not annotated");
            assert_eq!(
                away.as_deref(),
                Some("acct-b"),
                "the diverted session names where its one request actually went"
            );
        }
        _ => panic!("rows 1 and 2 should both be sessions"),
    }
}

#[test]
fn session_tree_folds_all_unpinned_into_one_trailing_row() {
    // One stable session builds the account tree; three fallback sessions
    // across two accounts must collapse into a single aggregate row.
    let sessions = vec![
        snap("a1", "acct-a", 3, Some(100)),
        unpinned("u1", "acct-a", 5, Some(90)),
        unpinned("u2", "acct-b", 7, Some(80)),
        unpinned("u3", "acct-a", 2, None), // never seen — must not win last_seen
    ];
    let rows = session_tree(&sessions);

    // acct-a header + its single stable session, then ONE unpinned aggregate.
    assert_eq!(rows.len(), 3);
    assert_eq!(header(&rows[0]), ("acct-a", 1));
    assert_eq!(session_id(&rows[1]), "a1");
    match &rows[2] {
        TreeRow::Unpinned {
            count,
            requests,
            last_seen,
        } => {
            assert_eq!(*count, 3); // three fallback conns, account-agnostic
            assert_eq!(*requests, 14); // 5 + 7 + 2
            assert_eq!(*last_seen, Some(at(90))); // youngest across the set
        }
        _ => panic!("row 2 should be the unpinned aggregate"),
    }
}

#[test]
fn session_tree_unpinned_row_renders_last() {
    // A fallback session leads the recency-ordered input, yet the aggregate
    // must still render AFTER every account group.
    let sessions = vec![
        unpinned("u1", "acct-a", 1, Some(100)),
        snap("a1", "acct-a", 1, Some(90)),
        snap("b1", "acct-b", 1, Some(80)),
    ];
    let rows = session_tree(&sessions);

    let last = rows.last().expect("rows are non-empty");
    assert!(
        matches!(last, TreeRow::Unpinned { count: 1, .. }),
        "the unpinned aggregate must be the final row"
    );
    // Nothing before the final row is an unpinned aggregate.
    for row in &rows[..rows.len() - 1] {
        assert!(
            !matches!(row, TreeRow::Unpinned { .. }),
            "only the last row may be the unpinned aggregate"
        );
    }
}

#[test]
fn session_tree_no_unpinned_row_when_all_stable() {
    let sessions = vec![
        snap("a1", "acct-a", 1, Some(10)),
        snap("a2", "acct-a", 1, Some(9)),
    ];
    let rows = session_tree(&sessions);
    assert!(
        !rows.iter().any(|r| matches!(r, TreeRow::Unpinned { .. })),
        "all-stable input must yield no unpinned row"
    );
}

/// A Codex account's sessions header keeps the end of its name, where `/codex` tells two
/// seats of one login apart, and in a fleet of both providers says ` codex` after the count,
/// dim. A Codex divert keeps its suffix the same way. Claude headers and diverts are drawn as
/// they always were.
#[test]
fn codex_sessions_carry_a_mark_and_keep_their_suffix() {
    let codex_session = |id: &str, served_by: &str| SessionSnapshot {
        provider: Provider::Codex,
        ..diverted(id, "bob@example.com/codex", served_by, 7, Some(100))
    };
    let draw = |snapshot: &StatsSnapshot| {
        let mut terminal =
            Terminal::new(TestBackend::new(48, 8)).expect("test backend builds a terminal");
        terminal
            .draw(|frame| render_sessions(frame, frame.area(), snapshot, at(160)))
            .expect("render succeeds");
        terminal.backend().buffer().clone()
    };

    let mut mixed = snapshot_of(vec![
        snap_gate("alice@example.com", GateReason::Ok, None),
        codex_gate("bob@example.com/codex", GateReason::Ok, None),
    ]);
    mixed.sessions = vec![
        codex_session("c0de", "carol@example.com/codex"),
        diverted(
            "a1f3",
            "alice@example.com",
            "frank@example.com",
            4,
            Some(90),
        ),
    ];
    let buffer = draw(&mixed);
    let rows = buffer_rows(&buffer);
    let (y, row) = row_holding(&rows, "▾ bob@exa…/codex · 1 codex");
    let y = y as u16;
    let mark = cell_x(row, "· 1 codex") + 4;
    for x in mark..mark + 5 {
        assert!(
            buffer[(x, y)].modifier.contains(Modifier::DIM),
            "the mark is dim: {row}"
        );
    }
    let name = cell_x(row, "bob@exa");
    assert!(
        !buffer[(name, y)].modifier.contains(Modifier::DIM),
        "the name is not: {row}"
    );
    row_holding(&rows, "▾ alice@example.com · 1");
    row_holding(&rows, "→car…/codex");
    row_holding(&rows, "→frank@exa…");

    let mut codex_only = snapshot_of(vec![codex_gate(
        "bob@example.com/codex",
        GateReason::Ok,
        None,
    )]);
    codex_only.sessions = vec![codex_session("c0de", "bob@example.com/codex")];
    let rows = buffer_rows(&draw(&codex_only));
    let (_, row) = row_holding(&rows, "▾ bob@example.c…/codex · 1");
    assert!(
        !row.contains("· 1 codex"),
        "no mark without Claude rows: {row}"
    );
}

/// A Codex request's path is long and its end is what tells one call from another, so the
/// log keeps its last two segments behind `…/`. Every Claude path is cut as it always was.
#[test]
fn codex_log_path_keeps_its_tail() {
    let mut snapshot = snapshot_of(vec![]);
    snapshot.recent = vec![
        crate::records::stats::RequestLogEntry {
            provider: Provider::Codex,
            ..log_entry(
                200,
                "POST",
                "/backend-api/codex/responses",
                "bob@example.com/codex",
                anchor(),
            )
        },
        log_entry(200, "POST", "/v1/messages", "alice@example.com", anchor()),
        log_entry(
            200,
            "GET",
            "/v1/messages/count_tokens?beta=true",
            "alice@example.com",
            anchor(),
        ),
    ];
    let mut terminal =
        Terminal::new(TestBackend::new(120, 6)).expect("test backend builds a terminal");
    terminal
        .draw(|frame| render_log(frame, frame.area(), &snapshot, anchor()))
        .expect("render succeeds");
    let rows = buffer_rows(terminal.backend().buffer());
    let (_, codex) = row_holding(&rows, "…/codex/responses");
    assert!(codex.contains("bob@example.com/codex"), "{codex}");
    row_holding(&rows, "/v1/messages ");
    row_holding(&rows, "/v1/messages/count_toke…");
}

#[test]
fn render_sessions_keeps_a_diverted_session_under_its_pin() {
    // One session pinned to `alice` whose most recent request was diverted to
    // `bob`. The pane must show it as alice's — with the divert marked — and
    // must NOT open a bob group, because no pin moved.
    let snapshot = StatsSnapshot {
        accounts: vec![],
        current: None,
        current_by_provider: crate::records::stats::CurrentByProvider::default(),
        recent: vec![],
        sessions: vec![diverted("a1f3", "alice", "bob", 24, Some(100))],
        wire_sessions: vec![],
        wire_sessions_summary: tcr_status_wire::SessionsSummary::default(),
    };
    let backend = TestBackend::new(48, 8);
    let mut terminal = Terminal::new(backend).expect("test backend builds a terminal");
    terminal
        .draw(|frame| render_sessions(frame, frame.area(), &snapshot, at(160)))
        .expect("render succeeds");
    let rows = buffer_rows(terminal.backend().buffer());
    let text = rows.join("\n");

    assert!(
        text.contains("▾ alice · 1"),
        "the session is grouped under its PINNED account\n{text}"
    );
    assert!(
        !text.contains("▾ bob"),
        "an account that merely served one diverted request gets NO group\n{text}"
    );
    let pin_row = rows
        .iter()
        .position(|row| row.contains("▾ alice"))
        .expect("the alice header is drawn");
    let session_row = rows
        .iter()
        .position(|row| row.contains("a1f3"))
        .expect("the session row is drawn");
    assert!(
        session_row > pin_row,
        "the session nests beneath its pin's header\n{text}"
    );
    assert!(
        rows[session_row].contains("→bob"),
        "the divert stays visible as a marker on the row\n{text}"
    );
}
