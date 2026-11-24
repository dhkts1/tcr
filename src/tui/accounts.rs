//! The accounts pane: its two column layouts, the Claude and Codex sections with the
//! divider between them, and the cells of one account row.

use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use time::OffsetDateTime;

use crate::provider::Provider;
use crate::stats::{AccountSnapshot, GateReason, StatsSnapshot};

use super::cells::{
    bar, bar_with_reset, fmt_age_opt, fmt_cache_ratio, fmt_tokens, gate_chip, pct, probe_cell,
    quota_cell, status_style, truncate,
};
use super::facts::DashboardFacts;
use super::input::{display_order, key_target};

/// Which of the two accounts-table layouts a given pane width gets. The choice
/// is a *deterministic breakpoint*, not the constraint solver's squeeze: ratatui
/// silently shrinks over-wide `Length` columns to make a too-wide table fit, and
/// the first casualty is the trailing `%` of each utilization bar — the exact
/// number Gil needs most on a small screen. An honest display degrades by
/// *choosing* what to drop (here the Probe/Cache columns, and the bars in favour
/// of bare percentages) rather than letting the solver clip a number silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AccountsLayout {
    /// The full 14-column table, unchanged. Chosen at or above
    /// [`FULL_LAYOUT_MIN_WIDTH`].
    Full,
    /// The reduced 11-column table: Probe/Cache dropped, the three quota buckets
    /// rendered as bar-less right-aligned percentages — the number IS the cell,
    /// so it survives any further squeeze. Chosen below [`FULL_LAYOUT_MIN_WIDTH`].
    Compact,
}

/// The Full layout's column headers, in render order. Paired BY INDEX with
/// [`FULL_COLUMN_WIDTHS`]; `full_layout_header_and_widths_are_paired` holds the
/// two lengths equal, because a header added without its width is the same class
/// of drift as a width added without its constant.
pub(super) const FULL_COLUMNS: [&str; 14] = [
    "Account", "Pri", "Status", "Gate", "Probe", "5h", "7d", "Fable", "Reqs", "In", "Cache", "Out",
    "Last", "Err",
];

/// The Full layout's declared column widths, paired by index with
/// [`FULL_COLUMNS`]. Sized to their widest legitimate content — the reason each
/// non-obvious one is what it is:
///
/// * `Gate` (15) — the widest gate chip, `FABLE-7D 47h30m`.
/// * `Fable` (15) — a learned bar is `[########] 100%`; 14 clipped the `%`.
/// * `5h` (22) — the bar plus a right-aligned `{:>6}` reset countdown from
///   [`bar_with_reset`] (`[########] 100%` + 1 separator + 6).
/// * `7d` (27) — the same bar+countdown plus a `near`/`full` quota label.
/// * `Cache` (7) — the hit ratio as a percentage, or `-` before any input.
/// * `Err` (4) — the decayed in-band SSE error count, or `-`.
pub(super) const FULL_COLUMN_WIDTHS: [u16; 14] = [18, 3, 9, 15, 11, 22, 27, 15, 6, 8, 7, 8, 6, 4];

/// The Account column's width: the Full layout's first column, which the
/// Compact layout reuses, so a name cell can be fitted to it once — see
/// [`codex_name_cell`].
const ACCOUNT_COLUMN_WIDTH: u16 = FULL_COLUMN_WIDTHS[0];

/// What the Account column leaves a name and its plan after the 2-cell marker.
const NAME_ROOM: usize = ACCOUNT_COLUMN_WIDTH as usize - 2;

/// The fewest characters of a name's head a cut may leave beside the plan. Fewer do not say
/// whose login the row is, so the plan gives way to the name instead.
const MIN_NAME_HEAD: usize = 3;

/// The usage windows a Codex row's `quota_windows` can list, by the status wire's names
/// (`quota::CodexWindows::wire_names`).
const FIVE_HOUR_WINDOW: &str = "fiveHour";
const SEVEN_DAY_WINDOW: &str = "sevenDay";

/// The narrowest terminal width that still fits the full 14-column table without
/// the constraint solver clipping a column. Kept a literal with the arithmetic
/// shown so a future column edit must update it *consciously* rather than
/// silently re-introducing the squeeze it exists to prevent:
///
/// ```text
///   159  Σ FULL_COLUMN_WIDTHS: 18+3+9+15+11+22+27+15+6+8+7+8+6+4
/// +  13  column_spacing (1 cell × 13 inter-column gaps)
/// +   2  the block's left + right borders
/// = 174
/// ```
///
/// The arithmetic is not taken on faith: `full_layout_min_width_fits_every_column`
/// RENDERS the table at this width and reads back each column's realised width
/// from the buffer. It was 155 while the table had 13 columns; the `Err` column
/// landed without it, and at 155 the solver silently took `Gate` down to 11 —
/// truncating exactly the `FABLE-7D 47h30m` label that column exists to show.
/// It went 160 → 174 when the `5h`/`7d` cells grew a reset countdown
/// ([`bar_with_reset`]), +7 each (1 separator + 6 countdown chars).
pub(super) const FULL_LAYOUT_MIN_WIDTH: u16 = 174;

/// Pick the accounts-table layout for a pane `width` — the pure, rendering-free
/// core of the responsive table, so the breakpoint is unit-testable without a
/// terminal. See [`AccountsLayout`] for *why* the breakpoint is deterministic.
pub(super) const fn accounts_layout(width: u16) -> AccountsLayout {
    if width >= FULL_LAYOUT_MIN_WIDTH {
        AccountsLayout::Full
    } else {
        AccountsLayout::Compact
    }
}

/// The Account cell: the selection marker, the name, and the plan dim beside it.
///
/// An Anthropic row is drawn exactly as it always was, its name and plan clipped by the column.
/// A Codex row is fitted to the column by [`codex_name_cell`] instead, because its name says
/// which seat of a login it is at the END (`/codex`, `/codex-2`), the part a clip would cut.
fn account_name_cell(marker: &str, account: &AccountSnapshot) -> Cell<'static> {
    let (name, plan) = match account.provider {
        Provider::Anthropic => (account.name.clone(), account.plan_label()),
        Provider::Codex => codex_name_cell(&account.name, short_plan(account).as_deref()),
    };
    let text = format!("{marker}{name}");
    match plan {
        Some(plan) => Cell::from(Line::from(vec![
            Span::raw(text),
            Span::styled(
                format!(" {plan}"),
                Style::default().add_modifier(Modifier::DIM),
            ),
        ])),
        None => Cell::from(text),
    }
}

/// A Codex row's plan, short: its label without the leading `ChatGPT ` (`Pro`, `Plus`,
/// `Team`, `Business`). The section already says these are ChatGPT accounts, and the six
/// characters are what the name needs.
fn short_plan(account: &AccountSnapshot) -> Option<String> {
    account
        .plan_label()
        .map(|label| match label.strip_prefix("ChatGPT ") {
            Some(short) => short.to_string(),
            None => label,
        })
}

/// A Codex row's name fitted to the [`NAME_ROOM`] cells after the marker, beside its short
/// plan: the name's text, and the plan to draw dim after it, or `None` when it gave way.
///
/// The name gets `NAME_ROOM` less the plan and its space. A name that fits is drawn whole. One
/// that does not keeps its trailing `/suffix` whole, since that tells two seats of one login
/// apart, and its head is cut with `…` (`bob@e…/codex`); a name with no `/` is cut at its end
/// ([`truncate`]). When the cut would leave fewer than [`MIN_NAME_HEAD`] characters of the head,
/// the plan is dropped and the name gets the whole room, and a name that still cannot keep that
/// much of its head is cut at its end.
pub(super) fn codex_name_cell(name: &str, short_plan: Option<&str>) -> (String, Option<String>) {
    if let Some(plan) = short_plan {
        let room = NAME_ROOM.saturating_sub(1 + plan.chars().count());
        if let Some(text) = fit_name(name, room) {
            return (text, Some(plan.to_string()));
        }
    }
    (cut_name(name, NAME_ROOM), None)
}

/// A Codex account's `name` in `room` cells: [`fit_name`], or a plain [`truncate`] when the
/// suffix would leave too little of the head to read.
pub(super) fn cut_name(name: &str, room: usize) -> String {
    fit_name(name, room).unwrap_or_else(|| truncate(name, room))
}

/// `name` in `room` cells: whole when it fits, else its trailing `/suffix` kept and its head
/// cut with `…`, or `None` when that leaves fewer than [`MIN_NAME_HEAD`] head characters. A
/// name with no `/` has an empty suffix, so this is [`truncate`] for it.
fn fit_name(name: &str, room: usize) -> Option<String> {
    if name.chars().count() <= room {
        return Some(name.to_string());
    }
    let (head, suffix) = name.rfind('/').map_or((name, ""), |at| name.split_at(at));
    let head_room = room.checked_sub(suffix.chars().count() + 1)?;
    (head_room >= MIN_NAME_HEAD).then(|| {
        let head: String = head.chars().take(head_room).collect();
        format!("{head}…{suffix}")
    })
}

/// Does this Codex row's plan lack the usage window `window` ([`FIVE_HOUR_WINDOW`],
/// [`SEVEN_DAY_WINDOW`])? Only a list the plan reported says so: a row whose windows are not
/// known yet lacks none, and an Anthropic row never does.
fn plan_lacks(account: &AccountSnapshot, window: &str) -> bool {
    match account.provider {
        Provider::Anthropic => false,
        Provider::Codex => account
            .codex
            .quota_windows
            .as_ref()
            .is_some_and(|windows| !windows.iter().any(|w| w == window)),
    }
}

/// The cell of a usage window the plan does not have: `none`, dim. Not `—`, which keeps
/// meaning "not measured yet".
fn none_cell() -> Cell<'static> {
    Cell::from("none").style(Style::default().add_modifier(Modifier::DIM))
}

/// The accounts pane with no facts beyond the snapshot. See [`render_accounts_with`], which
/// [`render_with`] draws through.
#[cfg(test)]
pub(super) fn render_accounts(
    frame: &mut Frame,
    area: Rect,
    snapshot: &StatsSnapshot,
    selected: usize,
    now: OffsetDateTime,
) {
    render_accounts_with(
        frame,
        area,
        snapshot,
        selected,
        &DashboardFacts::default(),
        now,
    );
}

/// Full height (rows) the accounts panel wants: one row per account + a header
/// row + top/bottom borders.
pub(super) const ACCOUNTS_CHROME: u16 = 3;

/// The accounts panel title: honest about how many accounts are actually on
/// screen. `" tcr · accounts (7) "` when all fit, or
/// `" tcr · accounts (5/7 ▼) "` when rows are clipped.
pub(super) fn accounts_title(shown: u16, total: u16) -> String {
    if shown >= total {
        format!(" tcr · accounts ({total}) ")
    } else {
        format!(" tcr · accounts ({shown}/{total} ▼) ")
    }
}

/// The rows a mixed pane spends on anything but accounts: its title border, the column
/// header, the divider between the two sections, and the bottom border.
pub(super) const MIXED_ACCOUNTS_CHROME: u16 = ACCOUNTS_CHROME + 1;

/// The border of the Codex section in a mixed pane. Its top is the divider: drawn as part of
/// the border, it costs one row and reads as one pane split in two, where a second block would
/// cost two and read as two panes.
const DIVIDER: symbols::border::Set = symbols::border::Set {
    top_left: symbols::line::VERTICAL_RIGHT,
    top_right: symbols::line::VERTICAL_LEFT,
    ..symbols::border::PLAIN
};

/// A section's title: ` {label} ({total}) `, or ` {label} ({shown}/{total} ▼) ` when rows are
/// clipped, as [`accounts_title`] reads for a pane of one provider.
fn section_title(label: &str, shown: u16, total: u16) -> String {
    if shown >= total {
        format!(" {label} ({total}) ")
    } else {
        format!(" {label} ({shown}/{total} ▼) ")
    }
}

/// The accounts table. Responsive: [`AccountsLayout::Full`] renders all 14
/// columns; [`AccountsLayout::Compact`] (below [`FULL_LAYOUT_MIN_WIDTH`]) drops
/// Probe/Cache and renders the quota buckets as bar-less percentages so the
/// utilization numbers stay visible on a narrow pane instead of being silently
/// clipped by the constraint solver.
///
/// One pane, in two sections when the fleet holds both providers: the Claude rows under the
/// pane's title and the column header, then a divider drawn into the border, ` codex (n) `,
/// and the Codex rows, in columns that line up with the ones above. Each section marks the
/// account serving its own provider. When the rows do not fit, [`split_rows`] decides how many
/// each section keeps. A fleet of one provider is one section: a Claude-only pane is exactly
/// the pane it always was, and a Codex-only one is titled as such.
///
/// `selected` is a DISPLAY row, an index into [`display_order`].
pub(super) fn render_accounts_with(
    frame: &mut Frame,
    area: Rect,
    snapshot: &StatsSnapshot,
    selected: usize,
    facts: &DashboardFacts,
    now: OffsetDateTime,
) {
    let layout = accounts_layout(area.width);
    let order = display_order(snapshot);
    let first_codex = order
        .iter()
        .position(|&pos| match snapshot.accounts[pos].provider {
            Provider::Anthropic => false,
            Provider::Codex => true,
        })
        .unwrap_or(order.len());
    let (claude, codex) = order.split_at(first_codex);
    let under_cursor = key_target(&order, selected).map(|pos| &snapshot.accounts[pos]);
    // The pane's bottom border is the last block's, whichever path draws it.
    let hint =
        under_cursor.and_then(|account| relogin_hint(account, area.width, facts.home.as_deref()));

    if claude.is_empty() || codex.is_empty() {
        // Rows that actually fit = panel height minus header + 2 borders, clamped to
        // the pool size. When `shown < total` the title flags the clip so a hidden
        // account is never silent.
        let total = order.len() as u16;
        let shown = area.height.saturating_sub(ACCOUNTS_CHROME).min(total);
        let title = if codex.is_empty() {
            accounts_title(shown, total)
        } else {
            section_title("tcr · accounts · codex", shown, total)
        };
        let mut block = Block::default().borders(Borders::ALL).title(title);
        if let Some(hint) = hint {
            block = block.title_bottom(hint);
        }
        let table = Table::new(
            section_rows(snapshot, &order, 0, selected, layout, now),
            accounts_widths(layout),
        )
        .header(accounts_header(layout))
        .block(block);
        frame.render_widget(table, area);
        if let (true, Some(login)) = (order.is_empty(), &facts.codex_login) {
            render_codex_login_hint(frame, area, login, facts.home.as_deref());
        }
        return;
    }

    let (claude_shown, codex_shown) = split_rows(
        area.height.saturating_sub(MIXED_ACCOUNTS_CHROME),
        claude.len() as u16,
        codex.len() as u16,
        under_cursor.is_some_and(|account| match account.provider {
            Provider::Anthropic => false,
            Provider::Codex => true,
        }),
    );
    // The Claude section takes its title, the header and its rows; the Codex section takes
    // the rest of the pane, so its bottom border is the pane's.
    let claude_area = Rect {
        height: area.height.min(2 + claude_shown),
        ..area
    };
    let codex_area = Rect {
        y: area.y + claude_area.height,
        height: area.height - claude_area.height,
        ..area
    };
    let claude_table = Table::new(
        section_rows(
            snapshot,
            &claude[..usize::from(claude_shown)],
            0,
            selected,
            layout,
            now,
        ),
        accounts_widths(layout),
    )
    .header(accounts_header(layout))
    .block(
        Block::default()
            .borders(Borders::TOP | Borders::LEFT | Borders::RIGHT)
            .title(section_title(
                "tcr · accounts · claude",
                claude_shown,
                claude.len() as u16,
            )),
    );
    frame.render_widget(claude_table, claude_area);
    let codex_table = Table::new(
        section_rows(
            snapshot,
            &codex[..usize::from(codex_shown)],
            claude.len(),
            selected,
            layout,
            now,
        ),
        accounts_widths(layout),
    )
    .block({
        let block = Block::default()
            .borders(Borders::ALL)
            .border_set(DIVIDER)
            .title(section_title("codex", codex_shown, codex.len() as u16));
        match hint {
            Some(hint) => block.title_bottom(hint),
            None => block,
        }
    });
    frame.render_widget(codex_table, codex_area);
}

/// The hint the pane's bottom border carries while the cursor is on a signed-out Codex row:
/// ` {name} needs re-login: {why} — tcr login --codex --account {name} `, in DIM. `None` on
/// any other row.
///
/// The name and the command are what a person acts on, so they always stay whole: the why is
/// cut, with a visible `…`, to the room `width` leaves after them, the two corners and the
/// title's two spaces. With fewer than two cells left for it, or no recorded login error to
/// word, the `: {why}` part is left out.
fn relogin_hint(
    account: &AccountSnapshot,
    width: u16,
    home: Option<&std::path::Path>,
) -> Option<Line<'static>> {
    match account.provider {
        Provider::Anthropic => return None,
        Provider::Codex => {}
    }
    if account.gate != GateReason::Login {
        return None;
    }
    let name = &account.name;
    let lead = format!("{name} needs re-login");
    let command = format!(" — tcr login --codex --account {name}");
    let fixed = lead.chars().count() + ": ".chars().count() + command.chars().count();
    let room = usize::from(width).saturating_sub(4).saturating_sub(fixed);
    let text = match account.relogin_why(home) {
        Some(why) if room >= 2 => format!(" {lead}: {}{command} ", truncate(&why, room)),
        _ => format!(" {lead}{command} "),
    };
    Some(Line::from(Span::styled(
        text,
        Style::default().add_modifier(Modifier::DIM),
    )))
}

/// The two lines an empty pane shows under its header when the codex CLI has a login of its
/// own on this machine: how to add an account of either provider, and where that login is, with
/// the reminder that `tcr login --codex` gives tcr a login of its own rather than sharing it.
/// The login is found by its existence alone and never read.
fn render_codex_login_hint(
    frame: &mut Frame,
    area: Rect,
    login: &std::path::Path,
    home: Option<&std::path::Path>,
) {
    let inner = Block::default().borders(Borders::ALL).inner(area);
    let below_header = Rect {
        y: inner.y.saturating_add(1),
        height: inner.height.saturating_sub(1),
        ..inner
    };
    let path = crate::stats::short_path(&login.to_string_lossy(), home);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from("no accounts — tcr login (Claude) · tcr login --codex (Codex)"),
            Line::from(Span::styled(
                format!(
                    "found a codex login at {path} — tcr login --codex gives tcr a login of its own"
                ),
                Style::default().add_modifier(Modifier::DIM),
            )),
        ]),
        below_header,
    );
}

/// How many rows each section of a mixed pane shows, given `available` rows for data: each
/// keeps `min(rows, 2)`, so neither provider's accounts vanish, and the rest goes first to the
/// section holding the cursor, then to the other. When even those minimums do not fit, the
/// cursor's section is filled first, because the row under the cursor is the one a key acts on.
pub(super) fn split_rows(
    available: u16,
    claude: u16,
    codex: u16,
    cursor_in_codex: bool,
) -> (u16, u16) {
    // (the cursor's section, the other), and back.
    let (mine, theirs) = if cursor_in_codex {
        (codex, claude)
    } else {
        (claude, codex)
    };
    let (min_mine, min_theirs) = (mine.min(2), theirs.min(2));
    let (shown_mine, shown_theirs) = if min_mine + min_theirs > available {
        let shown_mine = mine.min(available);
        (shown_mine, theirs.min(available - shown_mine))
    } else {
        let rest = available - min_mine - min_theirs;
        let more_mine = (mine - min_mine).min(rest);
        let more_theirs = (theirs - min_theirs).min(rest - more_mine);
        (min_mine + more_mine, min_theirs + more_theirs)
    };
    if cursor_in_codex {
        (shown_theirs, shown_mine)
    } else {
        (shown_mine, shown_theirs)
    }
}

/// The accounts table's bold column header, for `layout`.
fn accounts_header(layout: AccountsLayout) -> Row<'static> {
    match layout {
        AccountsLayout::Full => Row::new(FULL_COLUMNS.to_vec()),
        AccountsLayout::Compact => Row::new(vec![
            "Account", "Pri", "Status", "Gate", "5h", "7d", "Fable", "Reqs", "In", "Out", "Last",
        ]),
    }
    .style(Style::default().add_modifier(Modifier::BOLD))
}

/// The accounts table's column widths, for `layout`. One list for every section, so the
/// columns of a mixed pane line up across its divider.
fn accounts_widths(layout: AccountsLayout) -> Vec<Constraint> {
    match layout {
        // Straight from the declared widths, so the table ratatui lays out and the
        // number `FULL_LAYOUT_MIN_WIDTH` is computed from can never be two things.
        AccountsLayout::Full => FULL_COLUMN_WIDTHS
            .iter()
            .map(|w| Constraint::Length(*w))
            .collect(),
        // 91 widths + 10 spacing + 2 borders ≈ 103 cols. The three bar columns
        // collapse to bare percentages and Probe/Cache are gone. The stream-error
        // column stays Full-only — Compact is already the narrow-terminal squeeze.
        AccountsLayout::Compact => vec![
            Constraint::Length(ACCOUNT_COLUMN_WIDTH),
            Constraint::Length(3),
            Constraint::Length(9),
            Constraint::Length(15),
            // 5h as a bare right-aligned percentage (` 47%`).
            Constraint::Length(4),
            // 7d percentage + its "near"/"full" label (fits `100% full`).
            Constraint::Length(9),
            // Fable percentage; the header word sets the width, not the value.
            Constraint::Length(5),
            Constraint::Length(6),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(6),
        ],
    }
}

/// The table rows of the accounts at `positions` (positions in the snapshot's list), the first
/// of them display row `first`: the one at `selected` reversed under the cursor.
fn section_rows(
    snapshot: &StatsSnapshot,
    positions: &[usize],
    first: usize,
    selected: usize,
    layout: AccountsLayout,
    now: OffsetDateTime,
) -> Vec<Row<'static>> {
    positions
        .iter()
        .enumerate()
        .map(|(i, &pos)| account_row(snapshot, pos, first + i == selected, layout, now))
        .collect()
}

/// The table row of the account at position `pos` of the snapshot's list. ▶ marks the account
/// serving its provider right now ([`StatsSnapshot::serving`]).
fn account_row(
    snapshot: &StatsSnapshot,
    pos: usize,
    under_cursor: bool,
    layout: AccountsLayout,
    now: OffsetDateTime,
) -> Row<'static> {
    let account = &snapshot.accounts[pos];
    let is_current = snapshot.serving(account.provider) == Some(pos);
    let marker = if is_current { "▶ " } else { "  " };
    // The weekly quota annotation: an account parked out of rotation on its
    // cap reads as "near"/"full" (yellow/red), while its Status stays
    // "active" — the red "error" is reserved for a dead cred. Same pairing
    // in both layouts (compact keeps the label on the bar-less percentage).
    let (quota_label, quota_style) = quota_cell(account.quota_state);
    // Why this account is out of rotation and when it returns — the
    // per-row half of the fleet banner (`OK` / `5H 47m` / `LOGIN` / …).
    let (gate_label, gate_style) = gate_chip(account, now);
    let last_used = fmt_age_opt(account.last_used, now);

    // Columns shared by both layouts, in their shared order.
    //
    // The plan rides IN the name cell, dim, rather than taking a column
    // of its own: it is a fact ABOUT the name — the thing that tells two
    // identically-named rows apart — and a column would cost width on
    // every row to say nothing about the fifteen that share a plan.
    // Absent (not "unknown") for an account that has never been
    // profiled, matching the plain-text renderer's omitted `plan=`. A
    // Codex row fits its name and short plan to the column: see
    // `codex_name_cell`.
    let name = account_name_cell(marker, account);
    // A Codex plan may not have a window at all (ChatGPT Pro reports only
    // its weekly one), and ChatGPT has no model-scoped weekly window, so
    // a Codex row's Fable cell is blank.
    let lacks_five_hour = plan_lacks(account, FIVE_HOUR_WINDOW);
    let lacks_seven_day = plan_lacks(account, SEVEN_DAY_WINDOW);
    let priority = Cell::from(account.priority.to_string());
    let status = Cell::from(account.status.clone()).style(status_style(&account.status));
    let gate = Cell::from(gate_label).style(gate_style);
    let reqs = Cell::from(account.requests.to_string());
    let input = Cell::from(fmt_tokens(account.input_tokens));
    let output = Cell::from(fmt_tokens(account.output_tokens));
    let last = Cell::from(last_used);

    let cells = match layout {
        // Full mode: the probe cell, the three 8-cell bars, and the cache
        // ratio — exactly the pre-responsive column set.
        AccountsLayout::Full => {
            let (probe_label, probe_style) = probe_cell(account, now);
            vec![
                name,
                priority,
                status,
                gate,
                Cell::from(probe_label).style(probe_style),
                if lacks_five_hour {
                    none_cell()
                } else {
                    Cell::from(bar_with_reset(
                        account.five_hour,
                        account.five_hour_reset,
                        now,
                    ))
                },
                if lacks_seven_day {
                    none_cell()
                } else {
                    Cell::from(format!(
                        "{}{quota_label}",
                        bar_with_reset(account.seven_day, account.seven_day_reset, now)
                    ))
                    .style(quota_style)
                },
                // Model-scoped weekly (the Fable `7d_oi` bucket). Visibility
                // only: it never gates shared rotation (`eligible` ignores
                // it), so no quota label — the gate chip already reads
                // `FABLE-7D` when it parks Fable. `—` until first learned.
                match account.provider {
                    Provider::Anthropic => Cell::from(bar(account.seven_day_oi)),
                    Provider::Codex => Cell::from(""),
                },
                reqs,
                input,
                Cell::from(fmt_cache_ratio(
                    account.cache_read_tokens,
                    account.input_tokens,
                )),
                output,
                last,
                // Stream failures — in-band SSE `error` events plus streams
                // that hit EOF without `message_stop` (decayed count),
                // observability only — never a routing input. `-` when none
                // observed.
                Cell::from(if account.stream_error_count > 0 {
                    account.stream_error_count.to_string()
                } else {
                    "-".to_string()
                }),
            ]
        }
        // Compact mode: Probe and Cache dropped; each quota bucket becomes a
        // bare percentage (`pct`) so the number itself is the cell and
        // survives further squeeze. The 7d bucket keeps its "near"/"full"
        // label + style, exactly as in full mode.
        AccountsLayout::Compact => vec![
            name,
            priority,
            status,
            gate,
            if lacks_five_hour {
                none_cell()
            } else {
                Cell::from(pct(account.five_hour))
            },
            if lacks_seven_day {
                none_cell()
            } else {
                Cell::from(format!("{}{quota_label}", pct(account.seven_day))).style(quota_style)
            },
            match account.provider {
                Provider::Anthropic => Cell::from(pct(account.seven_day_oi)),
                Provider::Codex => Cell::from(""),
            },
            reqs,
            input,
            output,
            last,
        ],
    };

    let mut row = Row::new(cells);
    if account.disabled {
        row = row.style(
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        );
    }
    if under_cursor {
        row = row.style(Style::default().add_modifier(Modifier::REVERSED));
    }
    row
}
