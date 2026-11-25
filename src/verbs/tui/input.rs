//! What a key does: the key map, the cursor over the table the last frame drew, and
//! the enable or disable toggle with the notice it raises when the change does not stick.

use std::time::Duration;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::base::provider::Provider;
use crate::manager::{DisablePersist, Manager};
use crate::records::stats::StatsSnapshot;

/// What a key event asks the loop to do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Action {
    None,
    Quit,
    Up,
    Down,
    Disable,
    Enable,
    /// Repaint the whole screen from scratch. The escape hatch for a screen that
    /// something outside ratatui has written over: see [`SCREEN_DIRTY`].
    Redraw,
}

/// How long a [`Notice`] stays on screen. Long enough to read a full line without
/// hunting for it, short enough that a stale warning never outlives the state it
/// describes.
pub(super) const NOTICE_SECONDS: u64 = 6;

/// A short-lived line above the accounts table, for the one thing the dashboard
/// otherwise cannot tell you: that the key you just pressed did not do what the
/// table now shows. `tracing` is redirected to a log file in TUI mode, so a failed
/// `disabled` write was reported only somewhere the user is not looking, while the
/// row happily rendered as benched.
pub(super) struct Notice {
    pub(super) text: &'static str,
    expires_at: std::time::Instant,
    /// The account the notice is ABOUT, by name — `None` when the keypress named
    /// no account at all. Every notice describes one row, so it must not be read
    /// against another: a warning raised for `alice` still on screen while `bob` is
    /// selected reads as a warning about `bob`.
    pub(super) account: Option<String>,
}

impl Notice {
    pub(super) fn new(text: &'static str, account: Option<String>) -> Self {
        Self {
            text,
            expires_at: std::time::Instant::now() + Duration::from_secs(NOTICE_SECONDS),
            account,
        }
    }
}

/// The notice to paint this frame: the current one while it is unexpired AND still
/// about the row on screen, then nothing. Pure so both rules are testable without a
/// terminal or a real clock.
///
/// `selected_account` is the name of the currently selected row (`None` when there
/// is no such row, which is also what a `NoSuchAccount` notice carries — so those
/// two agree rather than falling through).
pub(super) fn live_notice<'a>(
    notice: &'a Option<Notice>,
    now: std::time::Instant,
    selected_account: Option<&str>,
) -> Option<&'a str> {
    notice
        .as_ref()
        .filter(|n| now < n.expires_at)
        .filter(|n| n.account.as_deref() == selected_account)
        .map(|n| n.text)
}

/// The selected row forced back inside a table of `count` rows — `0` when the table
/// is empty.
fn clamp_selected(selected: usize, count: usize) -> usize {
    selected.min(count.saturating_sub(1))
}

/// The selection a key event leaves behind: clamped into the table FIRST, then
/// moved. Every key event goes through this, which is the whole point — the
/// ordering is the fix, so it lives in one pure function rather than in the shape
/// of the event loop.
///
/// `Down` advances with a `saturating_add` and deliberately may overshoot; the next
/// event pulls it back. Before, the 500ms repaint was the ONLY thing that clamped,
/// so a `Down` immediately followed by `d`/`e` reached `set_disabled` with an index
/// past the end of the table: nothing was benched, and the user got a "that account
/// row no longer exists" banner about a row plainly on screen.
pub(super) fn next_selection(selected: usize, count: usize, action: Action) -> usize {
    let selected = clamp_selected(selected, count);
    match action {
        Action::Up => selected.saturating_sub(1),
        Action::Down => selected.saturating_add(1),
        Action::None | Action::Quit | Action::Disable | Action::Enable | Action::Redraw => selected,
    }
}

/// One row of the accounts pane as it was drawn: the account's slot, the index every key acts
/// through ([`crate::records::stats::AccountSnapshot::slot`]), its name, which the notices name, and its
/// provider, which words them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DrawnRow {
    pub(super) idx: usize,
    pub(super) name: String,
    pub(super) provider: Provider,
}

/// The order the accounts pane draws a fleet in, as positions in its list: the Claude rows,
/// then the Codex rows, each in rotation order. A fleet of one provider is drawn in rotation
/// order, so nothing moves for it. The manager's own vector is never reordered: its indices are
/// what every pin, session and key holds.
pub(super) fn display_order_of(providers: &[Provider]) -> Vec<usize> {
    let section = |provider: Provider| match provider {
        Provider::Anthropic => 0,
        Provider::Codex => 1,
    };
    let mut order: Vec<usize> = (0..providers.len()).collect();
    // Stable, so each section keeps rotation order.
    order.sort_by_key(|&pos| section(providers[pos]));
    order
}

/// [`display_order_of`] for `snapshot`'s rows.
pub(super) fn display_order(snapshot: &StatsSnapshot) -> Vec<usize> {
    let providers: Vec<Provider> = snapshot.accounts.iter().map(|a| a.provider).collect();
    display_order_of(&providers)
}

/// The position in the snapshot's list that display row `selected` draws, or `None` past the
/// last row.
pub(super) fn key_target(order: &[usize], selected: usize) -> Option<usize> {
    order.get(selected).copied()
}

/// The table every key acts through: the rows of `snapshot`, in the order [`render_accounts_with`]
/// draws them ([`display_order`]), each naming its account by slot. Built from the same order
/// the pane is drawn from, and kept with the frame it was drawn for, so a key always acts on
/// the row the person sees under the cursor.
pub(super) fn drawn_rows(snapshot: &StatsSnapshot) -> Vec<DrawnRow> {
    display_order(snapshot)
        .into_iter()
        .map(|pos| {
            let account = &snapshot.accounts[pos];
            DrawnRow {
                idx: account.slot,
                name: account.name.clone(),
                provider: account.provider,
            }
        })
        .collect()
}

/// Where the cursor goes when the next frame's table replaces the one drawn: the row of the
/// SAME account, wherever it moved. An account appended to a mixed fleet joins its own section,
/// which moves every Codex row below it down one, and a row deleted above the cursor moves it
/// up one; following the slot keeps the cursor on the account the person put it on. When that
/// account is gone (or nothing was drawn yet), the row number is kept, clamped into the table.
///
/// A fleet of one provider only ever grows at its end, so there the cursor keeps its row
/// number, as it always did.
pub(super) fn reanchor(drawn: &[DrawnRow], selected: usize, next: &[DrawnRow]) -> usize {
    drawn
        .get(selected)
        .and_then(|row| next.iter().position(|candidate| candidate.idx == row.idx))
        .unwrap_or_else(|| clamp_selected(selected, next.len()))
}

/// `d` or `e` on display row `selected` of the table the screen shows: the account that row
/// draws is benched or put back, by its slot. No rebuild from the manager's live order, which
/// may have grown since the frame was drawn, and no lookup by name: a slot is never handed to
/// another account (appends go at the end, a delete keeps its slot, and a revive returns an
/// account to its own), and [`Manager::set_disabled`] refuses a slot deleted since the frame.
pub(super) fn toggle(
    manager: &Manager,
    drawn: &[DrawnRow],
    selected: usize,
    disabled: bool,
) -> Option<Notice> {
    match drawn.get(selected) {
        Some(row) => toggle_notice(manager.set_disabled(row.idx, disabled), disabled, Some(row)),
        None => toggle_notice(DisablePersist::NoSuchAccount, disabled, None),
    }
}

/// The notice `d` or `e` leaves on `row` after `persist`, worded for the row's provider
/// ([`DisablePersist::warning_for`]) and naming the row, so it is never read against another.
/// `None` when the outcome needs no word. With no row, it is the line that says so, naming no
/// account.
pub(super) fn toggle_notice(
    persist: DisablePersist,
    disabled: bool,
    row: Option<&DrawnRow>,
) -> Option<Notice> {
    let provider = row.map_or(Provider::Anthropic, |row| row.provider);
    persist
        .warning_for(disabled, provider)
        .map(|text| Notice::new(text, row.map(|row| row.name.clone())))
}

/// Map a key press to an [`Action`]. Key-release events (kitty/Windows) are
/// ignored so a keypress does not fire twice.
pub(super) fn key_action(key: &KeyEvent) -> Action {
    if key.kind == KeyEventKind::Release {
        return Action::None;
    }
    match key.code {
        KeyCode::Char('q' | 'Q') => Action::Quit,
        KeyCode::Char('c' | 'C') if key.modifiers.contains(KeyModifiers::CONTROL) => Action::Quit,
        // `r` for redraw, plus the Ctrl-L every other full-screen program answers
        // to. Either one repairs a screen some other writer has corrupted.
        KeyCode::Char('l' | 'L') if key.modifiers.contains(KeyModifiers::CONTROL) => Action::Redraw,
        KeyCode::Char('r' | 'R') => Action::Redraw,
        KeyCode::Up | KeyCode::Char('k') => Action::Up,
        KeyCode::Down | KeyCode::Char('j') => Action::Down,
        KeyCode::Char('d' | 'D') => Action::Disable,
        KeyCode::Char('e' | 'E') => Action::Enable,
        _ => Action::None,
    }
}
