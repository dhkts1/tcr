//! Tests of the keys, the cursor over the drawn table, the toggle and its notices.

use super::*;

#[test]
fn redraw_is_bound_to_r_and_ctrl_l() {
    let plain = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
    let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
    assert_eq!(key_action(&plain('r')), Action::Redraw);
    assert_eq!(key_action(&plain('R')), Action::Redraw);
    assert_eq!(key_action(&ctrl('l')), Action::Redraw);
    assert_eq!(key_action(&ctrl('L')), Action::Redraw);
    // Ctrl-C still quits: the new Ctrl- arm must not have shadowed it.
    assert_eq!(key_action(&ctrl('c')), Action::Quit);
    // A bare `l` is not a redraw, so a stray keypress cannot force a repaint.
    assert_eq!(key_action(&plain('l')), Action::None);
}

#[test]
fn redraw_does_not_move_the_selection() {
    assert_eq!(next_selection(2, 5, Action::Redraw), 2);
}

/// The dashboard draws Claude rows first, then Codex rows, each in rotation order, and maps
/// a display row back to the account it draws: the manager's own order is never changed,
/// because its indices are what every pin and session holds.
#[test]
fn display_row_maps_to_rotation_index() {
    use Provider::{Anthropic as A, Codex as X};
    let order = display_order_of(&[A, X, A, X]);
    assert_eq!(order, vec![0, 2, 1, 3]);
    assert_eq!(display_order_of(&[A, A, A]), vec![0, 1, 2]);
    assert_eq!(display_order_of(&[X, X]), vec![0, 1]);
    assert_eq!(key_target(&order, 1), Some(2));
    assert_eq!(key_target(&order, 9), None);

    let buffer = accounts_buffer(&interleaved_fleet(), FULL_LAYOUT_MIN_WIDTH, 12, 1);
    let rows = buffer_rows(&buffer);
    let (frank, _) = row_holding(&rows, "frank@");
    let (bob, _) = row_holding(&rows, "bob@");
    assert!(
        buffer[(1, frank as u16)]
            .modifier
            .contains(Modifier::REVERSED),
        "display row 1 is frank's, the second Claude row:\n{}",
        rows.join("\n")
    );
    assert!(
        !buffer[(1, bob as u16)]
            .modifier
            .contains(Modifier::REVERSED),
        "bob is rotation index 1, not display row 1"
    );
}

/// The table a key acts through is the one the screen shows: display row `n` of the last
/// frame names its account by slot.
#[test]
fn the_drawn_row_lookup_is_pure() {
    let drawn = drawn_rows(&interleaved_fleet());
    assert_eq!(
        drawn.get(1),
        Some(&DrawnRow {
            idx: 2,
            name: "frank@example.com".to_string(),
            provider: Provider::Anthropic,
        })
    );
    assert_eq!(drawn.get(4), None);
}

/// A manager over the inline `accounts` array, with no config file behind it.
fn manager_of(accounts: &str) -> Arc<Manager> {
    let config: crate::base::config::Config =
        serde_json::from_str(&format!(r#"{{"accounts": {accounts}}}"#))
            .expect("the inline test config parses");
    Manager::with_live_refresher(config, None)
}

/// Each account's name and whether it is benched, as the manager now holds them.
fn benched(manager: &Manager) -> Vec<(String, bool)> {
    manager
        .snapshot(OffsetDateTime::now_utc())
        .accounts
        .into_iter()
        .map(|a| (a.name, a.disabled))
        .collect()
}

const ALICE: &str = r#"{"name": "alice@example.com", "accessToken": "at-alice"}"#;
const BOB: &str = r#"{"name": "bob@example.com/codex", "provider": "codex", "accountUuid": "22222222-2222-4222-8222-222222222222", "accessToken": "at-bob"}"#;
const CAROL: &str = r#"{"name": "carol@example.com/codex", "provider": "codex", "accountUuid": "33333333-3333-4333-8333-333333333333", "accessToken": "at-carol"}"#;
const FRANK: &str = r#"{"name": "frank@example.com", "accessToken": "at-frank"}"#;

/// `d` benches the account under the cursor as the screen drew it, even when the rotation
/// grew between the frame and the key. An appended Claude account lands among the Claude
/// rows, so every Codex row moves down one: a key mapped through the live order would bench
/// whichever account moved into the cursor's place.
#[test]
fn a_key_acts_on_the_row_drawn_not_a_shifted_one() {
    let manager = manager_of(&format!("[{ALICE}, {BOB}, {FRANK}]"));
    let drawn = drawn_rows(&manager.snapshot(OffsetDateTime::now_utc()));
    let names: Vec<&str> = drawn.iter().map(|row| row.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "alice@example.com",
            "frank@example.com",
            "bob@example.com/codex"
        ]
    );
    manager.add_account(
        serde_json::from_str(r#"{"name": "carol@example.com", "accessToken": "at-carol"}"#)
            .expect("an account"),
    );

    toggle(&manager, &drawn, 2, true);
    assert_eq!(
        benched(&manager),
        [
            ("alice@example.com".to_string(), false),
            ("bob@example.com/codex".to_string(), true),
            ("frank@example.com".to_string(), false),
            ("carol@example.com".to_string(), false),
        ]
    );
}

/// The cursor stays on its ACCOUNT from one frame to the next. An append in a mixed fleet
/// moves the Codex rows down one, and a row deleted above the cursor moves it up one; a
/// Claude-only table only ever grows at its end, so there the cursor keeps its row, as it
/// always did.
#[test]
fn the_cursor_follows_its_account_across_an_append() {
    let row = |idx: usize, name: &str, provider: Provider| DrawnRow {
        idx,
        name: name.to_string(),
        provider,
    };
    let alice = row(0, "alice@example.com", Provider::Anthropic);
    let bob = row(1, "bob@example.com/codex", Provider::Codex);
    let frank = row(2, "frank@example.com", Provider::Anthropic);
    let carol = row(3, "carol@example.com", Provider::Anthropic);

    let drawn = [alice.clone(), frank.clone(), bob.clone()];
    assert_eq!(
        reanchor(
            &drawn,
            2,
            &[alice.clone(), frank.clone(), carol.clone(), bob.clone()]
        ),
        3,
        "bob moved down one, and the cursor with him"
    );
    assert_eq!(
        reanchor(&drawn, 1, &[frank.clone(), bob]),
        0,
        "alice was deleted above frank, and the cursor stays on frank"
    );
    assert_eq!(
        reanchor(
            &[alice.clone(), frank.clone()],
            1,
            &[alice.clone(), frank.clone(), carol]
        ),
        1,
        "a Claude-only table grows at its end"
    );
    assert_eq!(
        reanchor(&[], 5, &[alice, frank]),
        1,
        "nothing drawn yet: clamped"
    );
}

/// The notice a refused key raises names the row it was raised for, and a Codex row's
/// duplicate notice names the Codex remedy.
#[test]
fn toggle_notice_names_the_row_and_its_provider() {
    use crate::manager::DisablePersist;
    let bob = DrawnRow {
        idx: 1,
        name: "bob@example.com/codex".to_string(),
        provider: Provider::Codex,
    };
    let alice = DrawnRow {
        idx: 0,
        name: "alice@example.com".to_string(),
        provider: Provider::Anthropic,
    };

    let notice = toggle_notice(DisablePersist::Ambiguous, true, Some(&bob))
        .expect("a duplicate is never saved");
    assert_eq!(
        notice.text,
        "NOT SAVED: two config entries share this Codex login — remove the duplicate \
         (tcr remove <name>), or re-import it with --name"
    );
    assert_eq!(notice.account.as_deref(), Some("bob@example.com/codex"));

    let notice = toggle_notice(DisablePersist::Ambiguous, true, Some(&alice))
        .expect("a duplicate is never saved");
    assert_eq!(Some(notice.text), DisablePersist::Ambiguous.warning(true));
    assert!(notice.text.ends_with("give each its own orgUuid"));
    assert_eq!(notice.account.as_deref(), Some("alice@example.com"));

    let notice =
        toggle_notice(DisablePersist::NoSuchAccount, false, None).expect("no row is never saved");
    assert_eq!(
        Some(notice.text),
        DisablePersist::NoSuchAccount.warning(false)
    );
    assert_eq!(notice.account, None);

    assert!(toggle_notice(DisablePersist::Persisted, true, Some(&bob)).is_none());
}

/// A manager over the inline `accounts` array, its config written to a fresh file in the
/// temp folder, never a real one.
fn manager_on_disk(tag: &str, accounts: &str) -> (Arc<Manager>, PathBuf) {
    let config: crate::base::config::Config =
        serde_json::from_str(&format!(r#"{{"accounts": {accounts}}}"#))
            .expect("the inline test config parses");
    let path = std::env::temp_dir().join(format!(
        "tcr-tui-{tag}-{}-{}.json",
        std::process::id(),
        crate::now_ms()
    ));
    crate::base::config::save(&path, &config).expect("write the test config");
    (
        Manager::with_live_refresher(config, Some(path.clone())),
        path,
    )
}

/// Rewrite the config at `path` to hold only `accounts`, as `tcr remove` does while the
/// dashboard runs. The sleep moves the file's mtime, which is what the reload watches.
fn rewrite_config(path: &std::path::Path, accounts: &str) {
    std::thread::sleep(Duration::from_millis(20));
    let config: crate::base::config::Config =
        serde_json::from_str(&format!(r#"{{"accounts": {accounts}}}"#))
            .expect("the inline test config parses");
    crate::base::config::save(path, &config).expect("rewrite the test config");
}

/// A row deleted from the config keeps its slot and leaves the list, so the slot, never the
/// position, is what a key acts on, and a frame drawn before the delete cannot reach it.
#[test]
fn a_key_never_reaches_a_row_deleted_from_the_config() {
    let (manager, path) = manager_on_disk("deleted-row-key", &format!("[{ALICE}, {BOB}, {FRANK}]"));
    let before = drawn_rows(&manager.snapshot(OffsetDateTime::now_utc()));
    let slots = |rows: &[DrawnRow]| -> Vec<(String, usize)> {
        rows.iter().map(|row| (row.name.clone(), row.idx)).collect()
    };
    assert_eq!(
        slots(&before),
        [
            ("alice@example.com".to_string(), 0),
            ("frank@example.com".to_string(), 2),
            ("bob@example.com/codex".to_string(), 1),
        ]
    );

    rewrite_config(&path, &format!("[{BOB}, {FRANK}]"));
    let after = drawn_rows(&manager.snapshot(OffsetDateTime::now_utc()));
    assert_eq!(
        slots(&after),
        [
            ("frank@example.com".to_string(), 2),
            ("bob@example.com/codex".to_string(), 1),
        ],
        "the reload left alice out"
    );

    toggle(&manager, &after, 1, true);
    assert_eq!(
        benched(&manager),
        [
            ("bob@example.com/codex".to_string(), true),
            ("frank@example.com".to_string(), false),
        ]
    );

    let notice = toggle(&manager, &before, 0, false);
    assert_eq!(
        notice.map(|n| n.text),
        DisablePersist::NoSuchAccount.warning(false),
        "a key on a frame drawn before the delete reaches no row"
    );
    std::fs::remove_file(&path).ok();
}

/// The serving markers are positions in the list, so a row deleted above the account
/// serving codex moves its marker up with it rather than off the screen.
#[test]
fn one_cursor_marker_per_provider_past_a_deleted_row() {
    let (manager, path) =
        manager_on_disk("deleted-row-marker", &format!("[{ALICE}, {BOB}, {CAROL}]"));
    manager.set_current(0);
    manager.set_current(2);
    rewrite_config(&path, &format!("[{ALICE}, {CAROL}]"));
    let snapshot = manager.snapshot(OffsetDateTime::now_utc());
    let names: Vec<&str> = snapshot.accounts.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, ["alice@example.com", "carol@example.com/codex"]);

    let rows = buffer_rows(&accounts_buffer(&snapshot, FULL_LAYOUT_MIN_WIDTH, 10, 0));
    let marked: Vec<&String> = rows.iter().filter(|row| row.starts_with("│▶ ")).collect();
    assert_eq!(marked.len(), 2, "{}", rows.join("\n"));
    assert!(marked[0].contains("alice@"), "{}", marked[0]);
    assert!(marked[1].contains("carol@"), "{}", marked[1]);
    std::fs::remove_file(&path).ok();
}

/// The person who pressed `d` must find out the write failed WITHOUT reading a
/// log file. `tracing` goes to a file in TUI mode, so a failed persist was
/// invisible while the row kept rendering as benched — the warning has to be on
/// screen, and it has to cost a row rather than paint over the fleet banner.
#[test]
fn render_shows_a_failed_persist_notice_without_hiding_the_fleet_banner() {
    let snapshot = util_snapshot(QuotaState::Normal);
    let warning = DisablePersist::NoEntry
        .warning(true)
        .expect("a NoEntry persist must warn");

    let with = {
        let backend = TestBackend::new(120, 14);
        let mut terminal = Terminal::new(backend).expect("test backend builds a terminal");
        terminal
            .draw(|frame| render(frame, &snapshot, 0, Some(warning)))
            .expect("render succeeds");
        buffer_rows(terminal.backend().buffer())
    };
    assert!(
        with.iter().any(|row| row.contains("NOT SAVED")),
        "the failed persist must be on screen\n{}",
        with.join("\n")
    );
    assert!(
        with[0].contains("FLEET"),
        "the notice must not paint over the fleet banner\n{}",
        with.join("\n")
    );

    // And with nothing to say it costs no rows at all — the notice is added to
    // the layout only while it is live.
    let without = {
        let backend = TestBackend::new(120, 14);
        let mut terminal = Terminal::new(backend).expect("test backend builds a terminal");
        terminal
            .draw(|frame| render(frame, &snapshot, 0, None))
            .expect("render succeeds");
        buffer_rows(terminal.backend().buffer())
    };
    assert!(
        !without.iter().any(|row| row.contains("NOT SAVED")),
        "no notice may render when there is nothing to warn about"
    );
    assert_ne!(
        with[1], without[1],
        "the notice must SHIFT the body down by a row, not overwrite it"
    );
}

/// A notice is transient: it shows until it expires and then stops, so a stale
/// warning can never outlive the state it describes.
#[test]
fn a_notice_expires_and_stops_rendering() {
    let notice = Some(Notice::new("NOT SAVED: test", Some("alice".to_string())));
    let start = std::time::Instant::now();

    assert_eq!(
        live_notice(&notice, start, Some("alice")),
        Some("NOT SAVED: test")
    );
    assert_eq!(
        live_notice(
            &notice,
            start + Duration::from_secs(NOTICE_SECONDS - 1),
            Some("alice")
        ),
        Some("NOT SAVED: test"),
        "the notice must survive long enough to be read"
    );
    assert_eq!(
        live_notice(
            &notice,
            start + Duration::from_secs(NOTICE_SECONDS + 1),
            Some("alice")
        ),
        None,
        "the notice must clear itself once it expires"
    );
    assert_eq!(live_notice(&None, start, Some("alice")), None);
}

/// A notice describes ONE row, so it may only be painted while that row is the
/// one on screen. Unscoped, a warning raised for `alice` kept rendering after
/// the selection moved to `bob` — where it reads as a warning about `bob`, an
/// account nothing was ever attempted on.
#[test]
fn a_notice_is_scoped_to_the_account_it_was_raised_for() {
    let notice = Some(Notice::new("NOT SAVED: test", Some("alice".to_string())));
    let now = std::time::Instant::now();

    assert_eq!(
        live_notice(&notice, now, Some("alice")),
        Some("NOT SAVED: test"),
        "unexpired and about the selected row → painted"
    );
    assert_eq!(
        live_notice(&notice, now, Some("bob")),
        None,
        "a warning about alice must never be shown over bob's row"
    );
    assert_eq!(
        live_notice(&notice, now, None),
        None,
        "…nor when no row is selected at all"
    );

    // The one notice that names no account — `NoSuchAccount`, raised when the
    // selection pointed at nothing — pairs with the empty selection.
    let rowless = Some(Notice::new("NOT SAVED: no row", None));
    assert_eq!(live_notice(&rowless, now, None), Some("NOT SAVED: no row"));
    assert_eq!(live_notice(&rowless, now, Some("alice")), None);
}

/// **THE off-by-one guard.** `Down` advances with a `saturating_add` and the
/// 500ms repaint was the ONLY thing that pulled it back in range, so a `Down`
/// immediately followed by `d` handed `set_disabled` an index past the end of
/// the table: nothing was benched, and the user was told "that account row no
/// longer exists" about a row plainly on screen.
///
/// Asserted on `next_selection` rather than on `clamp_selected`, because the
/// bug was never in the clamp — it was in WHEN the clamp ran. A test of the
/// clamp alone passes with the fix reverted, which makes it no gate at all.
#[test]
fn a_key_acts_on_a_clamped_selection_not_on_an_overshot_one() {
    // The `Down` that overshoots: last row of a 3-row table → 3, out of range.
    assert_eq!(next_selection(2, 3, Action::Down), 3);

    // Whatever key comes NEXT must act on row 2, not on row 3.
    for action in [Action::Disable, Action::Enable, Action::None] {
        assert_eq!(
            next_selection(3, 3, action),
            2,
            "{action:?} must act on the last row, not on an index past the end"
        );
    }
    assert_eq!(
        next_selection(3, 3, Action::Up),
        1,
        "`Up` starts from the clamped row too, or it takes two presses to move one"
    );

    // Ordinary movement is untouched.
    assert_eq!(next_selection(1, 3, Action::Up), 0);
    assert_eq!(
        next_selection(0, 3, Action::Up),
        0,
        "no underflow at the top"
    );
    assert_eq!(next_selection(1, 3, Action::Down), 2);

    // An empty table has no row to select and must not underflow.
    assert_eq!(next_selection(4, 0, Action::Disable), 0);
    assert_eq!(next_selection(0, 0, Action::Up), 0);
}
