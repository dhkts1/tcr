//! Tests of the accounts pane: Codex names and cells, sections, markers and clipping.

use super::*;

/// A Codex row keeps the END of its name, where `/codex` and `/codex-2` tell two seats of one
/// login apart, and shows its plan short (`Pro`, not `ChatGPT Pro`) beside it. The provider
/// tag that used to stand there ate half the name and said nothing the section does not.
#[test]
fn codex_names_keep_their_suffix_and_plan() {
    let mut first = codex_gate("alice@example.com/codex", GateReason::Ok, None);
    first.organization_type = Some("pro".to_string());
    let mut second = codex_gate("alice@example.com/codex-2", GateReason::Ok, None);
    second.organization_type = Some("pro".to_string());
    let snapshot = snapshot_of(vec![first, second]);
    for width in [FULL_LAYOUT_MIN_WIDTH, 110] {
        let rows = buffer_rows(&accounts_buffer(&snapshot, width, 8, 0));
        for start in ["│  alice…/codex Pro ", "│  ali…/codex-2 Pro "] {
            assert!(
                rows.iter().any(|row| row.starts_with(start)),
                "no row starts {start:?} at {width} columns:\n{}",
                rows.join("\n")
            );
        }
    }

    for (name, plan, text, kept) in [
        (
            "bob@example.com/codex",
            Some("Pro"),
            "bob@e…/codex",
            Some("Pro"),
        ),
        (
            "erin@example.com/codex",
            Some("Team"),
            "erin…/codex",
            Some("Team"),
        ),
        (
            "carol@example.com",
            Some("Pro"),
            "carol@examp…",
            Some("Pro"),
        ),
        (
            "jo@example.com",
            Some("Business"),
            "jo@exa…",
            Some("Business"),
        ),
        // Fewer than three characters of the head would be left beside the plan, so the
        // plan gives way and the name gets the whole cell.
        (
            "alice@example.com/codex-2",
            Some("Business"),
            "alice@e…/codex-2",
            None,
        ),
        ("dana@example.com", None, "dana@example.com", None),
    ] {
        assert_eq!(
            codex_name_cell(name, plan),
            (text.to_string(), kept.map(str::to_string)),
            "{name} beside {plan:?}"
        );
    }
}

/// A plan that reports only its weekly window (ChatGPT Pro) reads `none` in the 5h cell,
/// dim: the window does not exist on that plan, which is not the same as not measured yet
/// (`—`). The weekly cell is drawn as any other.
#[test]
fn weekly_only_codex_row_reads_none() {
    let mut bob = codex_gate("bob@example.com/codex", GateReason::Ok, None);
    bob.codex.quota_windows = Some(vec!["sevenDay".to_string()]);
    bob.seven_day = Some(0.34);
    bob.seven_day_reset = Some(anchor() + TimeDuration::days(4) + TimeDuration::seconds(30));
    bob.five_hour = None;
    let snapshot = snapshot_of(vec![bob]);

    let full = accounts_buffer(&snapshot, FULL_LAYOUT_MIN_WIDTH, 6, 0);
    let rows = buffer_rows(&full);
    let (y, row) = row_holding(&rows, "bob@");
    assert!(row.contains("none"), "{row}");
    assert!(row.contains(&bar(Some(0.34))), "{row}");
    assert!(
        !row.contains(&bar(None)),
        "no cell of a weekly-only row reads as never measured: {row}"
    );
    let x = cell_x(row, "none");
    assert!(
        full[(x, y as u16)].modifier.contains(Modifier::DIM),
        "`none` is dim: {:?}",
        full[(x, y as u16)]
    );

    let compact = buffer_rows(&accounts_buffer(&snapshot, 110, 6, 0));
    let (_, row) = row_holding(&compact, "bob@");
    let after = &row[row.find("none").expect("compact reads none") + "none".len()..];
    assert!(after.contains(&pct(Some(0.34))), "{row}");
}

/// A plan that reports no usage window at all reads `none` in both window cells. The Fable
/// cell is blank on every Codex row: ChatGPT has no model-scoped weekly window.
#[test]
fn no_window_codex_row_reads_none_none() {
    let mut jo = codex_gate("jo@example.com", GateReason::Ok, None);
    jo.codex.quota_windows = Some(vec![]);
    let snapshot = snapshot_of(vec![jo]);

    let rows = buffer_rows(&accounts_buffer(&snapshot, FULL_LAYOUT_MIN_WIDTH, 6, 0));
    let header = &rows[1];
    let (_, row) = row_holding(&rows, "jo@");
    let cells: Vec<char> = row.chars().collect();
    let cell = |label: &str, width: usize| {
        let x = usize::from(cell_x(header, label));
        cells[x..x + width].iter().collect::<String>()
    };
    assert_eq!(cell("5h", 4), "none", "{row}");
    assert_eq!(cell("7d", 4), "none", "{row}");
    assert_eq!(cell("Fable", 15).trim(), "", "{row}");

    let compact = buffer_rows(&accounts_buffer(&snapshot, 110, 6, 0));
    let (_, row) = row_holding(&compact, "jo@");
    assert!(row.contains("none none"), "{row}");
}

/// Each section marks the account serving its own provider. One marker for the whole pane
/// showed only whichever provider served last, so the account serving the other one
/// vanished from the screen every time it did.
#[test]
fn one_cursor_marker_per_provider() {
    let mut snapshot = interleaved_fleet();
    snapshot.current_by_provider = crate::records::stats::CurrentByProvider {
        anthropic: Some(2),
        codex: Some(3),
    };
    snapshot.current = Some(3);
    let rows = buffer_rows(&accounts_buffer(&snapshot, FULL_LAYOUT_MIN_WIDTH, 12, 0));
    let marked: Vec<&String> = rows.iter().filter(|row| row.starts_with("│▶ ")).collect();
    assert_eq!(marked.len(), 2, "{}", rows.join("\n"));
    assert!(marked[0].contains("frank@"), "{}", marked[0]);
    assert!(marked[1].contains("dana@"), "{}", marked[1]);
    let divider = rows
        .iter()
        .position(|row| row.starts_with('├'))
        .expect("a mixed pane has a divider");
    let (frank, _) = row_holding(&rows, "frank@");
    let (dana, _) = row_holding(&rows, "dana@");
    assert!(frank < divider && divider < dana, "{}", rows.join("\n"));
}

/// The pane's titles: a mixed fleet's Claude section under the pane title, the Codex
/// section under a divider drawn into the border, and a Codex-only pane titled as such. A
/// Claude-only pane keeps the title it always had.
#[test]
fn pane_titles_split_by_provider() {
    let rows = buffer_rows(&accounts_buffer(
        &interleaved_fleet(),
        FULL_LAYOUT_MIN_WIDTH,
        12,
        0,
    ));
    let pane = rows.join("\n");
    assert!(
        rows[0].starts_with("┌ tcr · accounts · claude (2) "),
        "{pane}"
    );
    let divider = rows[1..]
        .iter()
        .find(|row| row.starts_with('├'))
        .unwrap_or_else(|| panic!("no divider:\n{pane}"));
    assert!(divider.starts_with("├ codex (2) "), "{pane}");
    assert!(divider.ends_with('┤'), "{pane}");
    assert!(rows[rows.len() - 1].starts_with('└'), "{pane}");

    let codex_only = snapshot_of(vec![
        codex_gate("bob@example.com/codex", GateReason::Ok, None),
        codex_gate("dana@example.com", GateReason::Ok, None),
    ]);
    let rows = buffer_rows(&accounts_buffer(&codex_only, FULL_LAYOUT_MIN_WIDTH, 12, 0));
    assert!(
        rows[0].starts_with("┌ tcr · accounts · codex (2) "),
        "{}",
        rows.join("\n")
    );

    let claude_only = snapshot_of(vec![
        snap_gate("alice@example.com", GateReason::Ok, None),
        snap_gate("frank@example.com", GateReason::Ok, None),
    ]);
    let rows = buffer_rows(&accounts_buffer(&claude_only, FULL_LAYOUT_MIN_WIDTH, 12, 0));
    assert!(
        rows[0].starts_with("┌ tcr · accounts (2) "),
        "{}",
        rows.join("\n")
    );
}

/// When the rows do not fit, each section keeps two (or all it has, if fewer), the rest goes
/// to the section holding the cursor and then to the other, and a section that cannot keep
/// even that is the cursor's to fill first. Each clipped section says so in its title.
#[test]
fn clipping_keeps_two_rows_per_section_and_gives_the_rest_to_the_cursor() {
    assert_eq!(split_rows(20, 3, 4, false), (3, 4));
    assert_eq!(split_rows(6, 5, 5, false), (4, 2));
    assert_eq!(split_rows(6, 5, 5, true), (2, 4));
    assert_eq!(split_rows(3, 5, 5, true), (0, 3));
    assert_eq!(split_rows(5, 1, 9, false), (1, 4));

    let mut accounts: Vec<AccountSnapshot> = ["alice", "erin", "frank"]
        .iter()
        .map(|name| snap_gate(&format!("{name}@example.com"), GateReason::Ok, None))
        .collect();
    accounts.extend(
        ["bob", "carol", "dana", "gil", "hal"]
            .iter()
            .map(|name| codex_gate(&format!("{name}@example.com/codex"), GateReason::Ok, None)),
    );
    // 9 rows: the title, the header, the divider and the bottom border leave 5 for data.
    // The cursor is on the second Codex row, display row 4.
    let rows = buffer_rows(&accounts_buffer(
        &snapshot_of(accounts),
        FULL_LAYOUT_MIN_WIDTH,
        9,
        4,
    ));
    let pane = rows.join("\n");
    assert!(
        rows[0].starts_with("┌ tcr · accounts · claude (2/3 ▼) "),
        "{pane}"
    );
    assert!(
        rows.iter().any(|row| row.starts_with("├ codex (3/5 ▼) ")),
        "{pane}"
    );
}

/// A signed-out Codex row under the cursor names, in the pane's bottom border, who needs a
/// re-login, why, and the command that does it. The why gives way first: the name and the
/// command are what a person copies, so they always stay whole.
#[test]
fn relogin_hint_keeps_name_and_command() {
    let fleet = |login_error: Option<&str>| {
        let mut erin = codex_gate("erin@example.com", GateReason::Login, None);
        erin.status = "error".to_string();
        erin.organization_type = Some("team".to_string());
        erin.codex.login_error = login_error.map(str::to_string);
        erin.codex.codex_home = Some("/Users/example/.codex".to_string());
        snapshot_of(vec![
            codex_gate("bob@example.com", GateReason::Ok, None),
            erin,
        ])
    };
    let facts = DashboardFacts {
        home: Some(PathBuf::from("/Users/example")),
        ..DashboardFacts::default()
    };
    let draw = |snapshot: &StatsSnapshot, width: u16, height: u16, selected: usize| {
        let mut terminal =
            Terminal::new(TestBackend::new(width, height)).expect("test backend builds a terminal");
        terminal
            .draw(|frame| {
                render_accounts_with(frame, frame.area(), snapshot, selected, &facts, anchor());
            })
            .expect("render succeeds");
        terminal.backend().buffer().clone()
    };
    let bottom = |buffer: &Buffer| buffer_rows(buffer).pop().expect("the pane has rows");
    // Three rows of data under the title and the header, then the bottom border.
    let height = 3 + ACCOUNTS_CHROME;

    let reused = fleet(Some("refresh_token_reused"));
    let narrow = draw(&reused, 110, height, 1);
    assert_eq!(
        bottom(&narrow),
        "└ erin@example.com needs re-login: the codex CLI in ~/.codex… — tcr login --codex --account erin@example.com ┘"
    );
    let y = height - 1;
    for x in 1..109 {
        assert!(
            narrow[(x, y)].modifier.contains(Modifier::DIM),
            "the hint's cell {x} is DIM"
        );
    }

    let wide = bottom(&draw(&reused, 174, height, 1));
    assert!(
        wide.starts_with(
            "└ erin@example.com needs re-login: the codex CLI in ~/.codex refreshed this shared login first — tcr login --codex --account erin@example.com "
        ),
        "{wide}"
    );
    assert!(wide.ends_with('┘'), "{wide}");

    let on_bob = bottom(&draw(&reused, 110, height, 0));
    assert!(!on_bob.contains("needs re-login"), "{on_bob}");

    let unknown = bottom(&draw(&fleet(None), 110, height, 1));
    assert!(
        unknown.starts_with(
            "└ erin@example.com needs re-login — tcr login --codex --account erin@example.com "
        ),
        "{unknown}"
    );
    assert!(unknown.ends_with('┘'), "{unknown}");

    // In a fleet of both providers the hint is on the Codex section's border, which is the
    // pane's bottom one, and only while the cursor is on the signed-out row.
    let mut mixed = fleet(Some("refresh_token_reused"));
    mixed
        .accounts
        .insert(0, snap_gate("alice@example.com", GateReason::Ok, None));
    let mixed = snapshot_of(mixed.accounts);
    let height = 3 + MIXED_ACCOUNTS_CHROME;
    let on_erin = bottom(&draw(&mixed, 110, height, 2));
    assert_eq!(
        on_erin,
        "└ erin@example.com needs re-login: the codex CLI in ~/.codex… — tcr login --codex --account erin@example.com ┘"
    );
    let on_alice = bottom(&draw(&mixed, 110, height, 0));
    assert!(!on_alice.contains("needs re-login"), "{on_alice}");
}
