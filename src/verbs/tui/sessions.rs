//! The sessions pane, each session grouped under the account it is pinned to, and the
//! recent-request log below it.

use std::collections::HashMap;

use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use time::OffsetDateTime;

use crate::base::provider::Provider;
use crate::records::stats::{SessionKind, SessionSnapshot, StatsSnapshot};

use super::accounts::cut_name;
use super::cells::{fmt_age, fmt_age_opt, status_color, truncate};
use super::facts::holds;

/// One row in the sessions pane's account→session tree: either an account
/// header carrying the group's aggregates, or one of that account's sessions.
pub(super) enum TreeRow {
    /// A group header: the account name, its session count, its summed request
    /// total, and the youngest `last_seen` across the group.
    Account {
        name: String,
        /// The provider of the account the group is pinned to, which decides how its name is
        /// cut and whether the header carries the ` codex` mark.
        provider: Provider,
        count: usize,
        requests: u64,
        last_seen: Option<OffsetDateTime>,
    },
    /// A single session under the account it is PINNED to.
    Session {
        id: String,
        requests: u64,
        last_seen: Option<OffsetDateTime>,
        /// The account that served this session's most recent request, when that
        /// is NOT its pinned account — i.e. the request was diverted while the pin
        /// was held. `None` on the normal case. The session still lives under its
        /// pinned account's header; this only annotates the row.
        diverted_to: Option<String>,
        /// The session's provider: a Codex session is only ever diverted to another Codex
        /// account, whose name is cut keeping its `/suffix`.
        provider: Provider,
    },
    /// The single collapsed aggregate for ALL [`SessionKind::Fallback`] sessions
    /// — those with no pin at all. [`SessionKind::Prefix`] sessions are NOT
    /// included here: they have a real pin (just a weaker one) and group under
    /// their account like `Stable` sessions do. Rendered LAST, dim, with no
    /// children, so if unpinned traffic is ever tracked it stays visible as one
    /// honest row instead of flooding the pane.
    ///
    /// As of this writing, UNREACHABLE from a live request:
    /// [`crate::manager::Manager::record_served`] only creates a session entry
    /// when its key is `Some`, and `handle` (`src/proxy/mod.rs`) never pairs
    /// `SessionKind::Fallback` with a `Some` key — the fallback branch always
    /// hands back `(None, SessionKind::Fallback)`. So a real request that ends
    /// up `Fallback` is simply never tracked as a session at all, and this row
    /// exercises only via `session_tree`'s own unit tests (`demo.rs` does not
    /// currently seed one either). Kept in case a future change starts tracking
    /// unpinned sessions, not because it renders today.
    Unpinned {
        count: usize,
        requests: u64,
        last_seen: Option<OffsetDateTime>,
    },
}

/// Group sessions into a PINNED-account→sessions tree. Any PINNED session
/// (`Stable` or `Prefix`) builds the tree: for each account — in order of its
/// FIRST appearance in the (stably ordered) input — emit an [`TreeRow::Account`]
/// header carrying the group's session count, its summed requests, and the
/// group's YOUNGEST `last_seen`, followed by that account's sessions in input
/// order. Grouping keys on [`SessionSnapshot::account`], the PIN, so a session
/// whose last request was merely diverted keeps its row under its own account
/// and is annotated with `diverted_to` instead of jumping to another group. Only
/// [`SessionKind::Fallback`] sessions — no pin at all — instead fold into one
/// trailing [`TreeRow::Unpinned`] aggregate, rendered LAST and only when
/// non-empty (see its doc-comment for why that path does not fire from a real
/// request today). Pure and terminal-free so it can be unit-tested directly.
pub(super) fn session_tree(sessions: &[SessionSnapshot]) -> Vec<TreeRow> {
    // Group PINNED (Stable or Prefix) members by account while recording each
    // account's first-appearance order — an empty freshly-inserted bucket marks
    // a not-yet-seen account. Only Fallback (no pin at all) sessions bypass the
    // tree and accumulate into the single unpinned aggregate.
    let mut order: Vec<&str> = Vec::new();
    let mut groups: HashMap<&str, Vec<&SessionSnapshot>> = HashMap::new();
    let mut unpinned_count = 0usize;
    let mut unpinned_requests = 0u64;
    let mut unpinned_last_seen: Option<OffsetDateTime> = None;
    for session in sessions {
        if session.kind == SessionKind::Fallback {
            unpinned_count += 1;
            unpinned_requests += session.requests;
            // Youngest last_seen across the fallback set; a never-seen session
            // (None) is skipped so it can never win over one that has been seen.
            if let Some(seen) = session.last_seen {
                unpinned_last_seen = Some(unpinned_last_seen.map_or(seen, |cur| cur.max(seen)));
            }
            continue;
        }
        let members = groups.entry(session.account.as_str()).or_default();
        if members.is_empty() {
            order.push(session.account.as_str());
        }
        members.push(session);
    }

    let mut rows = Vec::with_capacity(sessions.len() + order.len() + 1);
    for account in order {
        let members = &groups[account];
        let requests = members.iter().map(|s| s.requests).sum();
        // Youngest last_seen is the max instant; a never-seen session (None) is
        // filtered out so it can never win over one that has been seen.
        let last_seen = members.iter().filter_map(|s| s.last_seen).max();
        rows.push(TreeRow::Account {
            name: account.to_string(),
            // Every member is pinned to this one account, so they share its provider.
            provider: members[0].provider,
            count: members.len(),
            requests,
            last_seen,
        });
        for session in members {
            rows.push(TreeRow::Session {
                id: session.id.clone(),
                requests: session.requests,
                last_seen: session.last_seen,
                // Only a genuine divert annotates the row: the account that served
                // last differs from the one the session is pinned to.
                diverted_to: (session.last_served_account != session.account)
                    .then(|| session.last_served_account.clone()),
                provider: session.provider,
            });
        }
    }
    // The unpinned aggregate is always LAST and present only when non-empty.
    if unpinned_count > 0 {
        rows.push(TreeRow::Unpinned {
            count: unpinned_count,
            requests: unpinned_requests,
            last_seen: unpinned_last_seen,
        });
    }
    rows
}

/// The shared skeleton behind the two data panes ([`render_accounts`] and
/// [`render_sessions`]): a bold header row + caller-mapped rows + column widths,
/// wrapped in an all-borders block with `title`, rendered into `area`. Each
/// caller supplies its own column set and per-row cell builder (incl. any
/// per-row selection/highlight styling); this owns only the identical
/// `Table::new(...).header(...).block(...)` + `render_widget` tail.
fn render_table<'a>(
    frame: &mut Frame,
    area: Rect,
    header: Row<'a>,
    widths: Vec<Constraint>,
    rows: Vec<Row<'a>>,
    title: impl Into<Line<'a>>,
) {
    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(title.into()));
    frame.render_widget(table, area);
}

/// The live sessions pane, drawn as a PINNED-account→sessions tree: each account
/// holding pins becomes a Cyan+bold header `▾ <name> · <count>` carrying the
/// group's summed requests and youngest age, with its sessions indented beneath as
/// short ids — so load balance across accounts and each session's affinity read at
/// a glance. Rows arrive in a stable (account, id) order and [`session_tree`]
/// preserves it, so a row never moves because a request was served.
///
/// A session whose LAST request was diverted off its pin (a Fable title call, one
/// request during a short hold) keeps its row under its own account and gets a dim
/// `→<account>` suffix naming where that one request actually went. That is the
/// honest reading: the pin did not move, so neither does the row.
///
/// A Codex account's name is cut keeping its trailing `/suffix`, the part that tells two seats
/// of one login apart, in its header and in a divert. In a fleet of both providers its header
/// also carries a dim ` codex` mark after the count, and its name gives the mark the room:
/// 14 cells instead of 20, so the header still fits the 28-cell column. Claude headers and
/// diverts are drawn as they always were.
pub(super) fn render_sessions(
    frame: &mut Frame,
    area: Rect,
    snapshot: &StatsSnapshot,
    now: OffsetDateTime,
) {
    let header = Row::new(vec!["Session", "Reqs", "Last"])
        .style(Style::default().add_modifier(Modifier::BOLD));

    let widths = vec![
        Constraint::Length(28),
        Constraint::Length(6),
        Constraint::Length(8),
    ];

    let age = |seen: Option<OffsetDateTime>| fmt_age_opt(seen, now);
    let mixed = holds(snapshot, Provider::Anthropic) && holds(snapshot, Provider::Codex);

    let capacity = area.height.saturating_sub(3) as usize;
    let rows: Vec<Row> = if snapshot.sessions.is_empty() {
        // Empty pane reads clearly when affinity is off / nothing served yet.
        vec![Row::new(vec![
            Cell::from("(no active sessions)").style(Style::default().fg(Color::DarkGray)),
        ])]
    } else {
        session_tree(&snapshot.sessions)
            .into_iter()
            .take(capacity.max(1))
            .map(|row| match row {
                TreeRow::Account {
                    name,
                    provider,
                    count,
                    requests,
                    last_seen,
                } => Row::new(vec![
                    match (provider, mixed) {
                        (Provider::Anthropic, _) => {
                            Cell::from(format!("▾ {} · {count}", truncate(&name, 20)))
                        }
                        // 2 + 14 + 4 + 6 = 26 cells, inside the 28-cell column. The mark is a
                        // secondary fact, so it drops the header's bold.
                        (Provider::Codex, true) => Cell::from(Line::from(vec![
                            Span::raw(format!("▾ {} · {count}", cut_name(&name, 14))),
                            Span::styled(
                                " codex",
                                Style::default()
                                    .add_modifier(Modifier::DIM)
                                    .remove_modifier(Modifier::BOLD),
                            ),
                        ])),
                        (Provider::Codex, false) => {
                            Cell::from(format!("▾ {} · {count}", cut_name(&name, 20)))
                        }
                    },
                    Cell::from(requests.to_string()),
                    Cell::from(age(last_seen)),
                ])
                // Cyan + bold header matches the log/account cell styling.
                .style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                TreeRow::Session {
                    id,
                    requests,
                    last_seen,
                    diverted_to,
                    provider,
                } => {
                    // Four-space indent nests the session under its PINNED account.
                    let mut spans = vec![Span::raw(format!("    {}", truncate(&id, 10)))];
                    if let Some(account) = diverted_to {
                        // Dim so the row still reads as belonging to its pin: the
                        // session did not move, one request went elsewhere. Widths:
                        // 4 + 10 + 2 + 10 = 26, inside the 28-cell column.
                        let account = match provider {
                            Provider::Anthropic => truncate(&account, 10),
                            Provider::Codex => cut_name(&account, 10),
                        };
                        spans.push(Span::styled(
                            format!(" →{account}"),
                            Style::default()
                                .fg(Color::DarkGray)
                                .add_modifier(Modifier::DIM),
                        ));
                    }
                    Row::new(vec![
                        Cell::from(Line::from(spans)),
                        Cell::from(requests.to_string()),
                        Cell::from(age(last_seen)),
                    ])
                }
                TreeRow::Unpinned {
                    count,
                    requests,
                    last_seen,
                } => Row::new(vec![
                    Cell::from(format!("▸ (unpinned) · {count} conns")),
                    Cell::from(requests.to_string()),
                    Cell::from(age(last_seen)),
                ])
                // Dim gray de-emphasizes fallback traffic, matching disabled rows.
                .style(
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::DIM),
                ),
            })
            .collect()
    };

    render_table(frame, area, header, widths, rows, " sessions ");
}

/// The recent-request log pane. Rows are already most-recent-first.
pub(super) fn render_log(
    frame: &mut Frame,
    area: Rect,
    snapshot: &StatsSnapshot,
    now: OffsetDateTime,
) {
    let capacity = area.height.saturating_sub(2) as usize;
    let lines: Vec<Line> = snapshot
        .recent
        .iter()
        .take(capacity)
        .map(|entry| {
            let path = match entry.provider {
                Provider::Anthropic => entry.path.clone(),
                Provider::Codex => codex_log_path(&entry.path),
            };
            Line::from(vec![
                Span::styled(
                    format!("{:>3} ", entry.status),
                    Style::default().fg(status_color(entry.status)),
                ),
                Span::raw(format!("{:<6} ", entry.method)),
                Span::raw(format!("{:<24} ", truncate(&path, 24))),
                Span::styled(entry.account.clone(), Style::default().fg(Color::Cyan)),
                Span::raw(format!("  {} ago", fmt_age(now - entry.time))),
            ])
        })
        .collect();

    let log = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" recent · q quit · ↑↓/jk select · d/e disable/enable · r redraw "),
    );
    frame.render_widget(log, area);
}

/// A Codex request's path as the log shows it: its query dropped, and a path of more than two
/// segments shown as `…/` and its last two (`/backend-api/codex/responses` reads
/// `…/codex/responses`). The head is the same on every Codex call, and the 24-cell cut used to
/// take the end, which is the part that tells one call from another.
fn codex_log_path(path: &str) -> String {
    let path = path.split('?').next().unwrap_or_default();
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    match segments.as_slice() {
        [.., second_last, last] if segments.len() > 2 => format!("…/{second_last}/{last}"),
        _ => path.to_string(),
    }
}
