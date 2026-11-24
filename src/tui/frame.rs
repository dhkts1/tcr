//! The whole frame: the fleet banner and the notice row on top, then the accounts,
//! sessions and request-log panes, each given its share of the height.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use time::OffsetDateTime;

use crate::provider::Provider;
use crate::stats::StatsSnapshot;

use super::accounts::{ACCOUNTS_CHROME, render_accounts_with};
use super::banner::{alarm_style, render_fleet_banner};
use super::facts::{DashboardFacts, holds};
use super::sessions::{render_log, render_sessions};

/// Keep the sessions pane usable even when the terminal is short: the accounts
/// panel never eats so much height that fewer than this many rows remain for the
/// rest of the frame.
const SESSIONS_MIN: u16 = 3;

/// Height (rows) the accounts panel should get. It takes its full height whenever
/// at least `SESSIONS_MIN` rows remain for the rest of the frame; when the terminal
/// is too short for that, it yields down to `total_height - SESSIONS_MIN` (so the
/// recent-log shrinks/vanishes entirely before a single account row is dropped),
/// but never below a 4-row floor so a tiny terminal still shows the header plus at
/// least one account.
pub(super) fn account_area_height(total_height: u16, n_accounts: u16) -> u16 {
    let acct_full = n_accounts.saturating_add(ACCOUNTS_CHROME);
    acct_full
        .min(total_height.saturating_sub(SESSIONS_MIN))
        .max(4)
}

/// Paint the whole frame: accounts table on top, sessions pane in the middle,
/// request log below. No facts beyond the snapshot, at the clock's now: see
/// [`render_with`], which the run loop draws through with the facts it read.
#[cfg(test)]
pub(super) fn render(
    frame: &mut Frame,
    snapshot: &StatsSnapshot,
    selected: usize,
    notice: Option<&str>,
) {
    render_with(
        frame,
        snapshot,
        selected,
        notice,
        &DashboardFacts::default(),
        OffsetDateTime::now_utc(),
    );
}

/// [`render`], with the facts the run loop read this tick and the instant it read them at.
/// Draws only: it touches neither the filesystem nor the clock.
pub(super) fn render_with(
    frame: &mut Frame,
    snapshot: &StatsSnapshot,
    selected: usize,
    notice: Option<&str>,
    facts: &DashboardFacts,
    now: OffsetDateTime,
) {
    let area = frame.area();
    // A single-row fleet banner sits above everything — the fleet aggregate the
    // per-row table can't show (how many accounts are actually in rotation, and
    // when the first one returns when none are). The rest of the frame lays out
    // below it exactly as before.
    //
    // A transient notice takes ONE more row directly under it, and only while there
    // is something to say — it is added to the layout rather than painted over the
    // banner, so a warning never costs the fleet status it might explain.
    let notice_height = u16::from(notice.is_some());
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(notice_height),
            Constraint::Min(0),
        ])
        .split(area);
    render_fleet_banner(frame, outer[0], snapshot, facts, now);
    if let Some(text) = notice {
        render_notice(frame, outer[1], text);
    }
    let body = outer[2];
    // Accounts is the primary data: budget its height first so it is the LAST
    // thing clipped. SESSIONS absorbs the vertical slack (grows when the terminal
    // is tall); the recent-log lives in whatever remains, so it shrinks/vanishes
    // before any account row is dropped.
    let acct_h = account_area_height(body.height, accounts_pane_rows(snapshot, facts));
    let rest = body.height.saturating_sub(acct_h);
    let log_h = rest.saturating_sub(SESSIONS_MIN).min(9);
    let sessions_h = rest.saturating_sub(log_h);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(acct_h),
            Constraint::Length(sessions_h),
            Constraint::Length(log_h),
        ])
        .split(body);
    render_accounts_with(frame, chunks[0], snapshot, selected, facts, now);
    render_sessions(frame, chunks[1], snapshot, now);
    render_log(frame, chunks[2], snapshot, now);
}

/// The rows the accounts pane holds besides its title, header and bottom border: one per
/// account, and in a fleet of both providers one more for the divider between them, so a
/// divider never costs an account its row. An empty fleet with a codex login on disk holds the
/// two lines that point at it.
fn accounts_pane_rows(snapshot: &StatsSnapshot, facts: &DashboardFacts) -> u16 {
    let rows = snapshot.accounts.len() as u16;
    if rows == 0 && facts.codex_login.is_some() {
        2
    } else if holds(snapshot, Provider::Anthropic) && holds(snapshot, Provider::Codex) {
        rows + 1
    } else {
        rows
    }
}

/// The transient notice row. Painted in the alarm style, because every notice we
/// raise means an action the user believes happened did not.
fn render_notice(frame: &mut Frame, area: Rect, text: &str) {
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(text.to_string(), alarm_style()))),
        area,
    );
}
