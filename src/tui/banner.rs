//! The one-line fleet banner: how many accounts of each provider are in rotation, which
//! returns first when none are, and why Codex is not pooled.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use tcr_status_wire::CodexRoute;
use time::{Duration as TimeDuration, OffsetDateTime};

use crate::provider::Provider;
use crate::stats::{AccountSnapshot, GateReason, StatsSnapshot};

use super::cells::{rel, truncate};
use super::facts::{DashboardFacts, holds};

/// One row per account is the wrong altitude for "is the whole fleet up?" — the
/// live incident showed seven individually-busy rows and no aggregate. This
/// distilled fleet status is exactly that: how many accounts are in rotation, of
/// how many, and — only when NONE are — which account returns first and when.
pub(super) struct FleetStatus {
    pub(super) eligible: usize,
    pub(super) total: usize,
    /// The soonest-returning account (name + time-until), computed ONLY when
    /// `eligible == 0`. `None` there means every gated account has an unknown
    /// clear-instant, so the banner says "next free unknown" rather than lie.
    pub(super) next_free: Option<(String, TimeDuration)>,
}

/// Distil the account snapshots into a [`FleetStatus`]. An account is eligible
/// when its gate is [`GateReason::Ok`] and it is not disabled — the same hard
/// gates selection honours. When none are, the soonest known `free_at` names the
/// first account to return. Pure and terminal-free so it can be unit-tested.
pub(super) fn fleet_status(accounts: &[AccountSnapshot], now: OffsetDateTime) -> FleetStatus {
    fleet_status_of(accounts, now)
}

/// [`fleet_status`] over any set of rows, so the banner can take one per provider.
fn fleet_status_of<'a>(
    accounts: impl IntoIterator<Item = &'a AccountSnapshot>,
    now: OffsetDateTime,
) -> FleetStatus {
    let accounts: Vec<&AccountSnapshot> = accounts.into_iter().collect();
    let total = accounts.len();
    let eligible = accounts
        .iter()
        .filter(|a| a.gate == GateReason::Ok && !a.disabled)
        .count();
    let next_free = if eligible == 0 {
        accounts
            .iter()
            .filter_map(|a| a.free_at.filter(|&f| f > now).map(|f| (a.name.clone(), f)))
            .min_by_key(|&(_, f)| f)
            .map(|(name, f)| (name, f - now))
    } else {
        None
    };
    FleetStatus {
        eligible,
        total,
        next_free,
    }
}

/// The single-row fleet banner: how many accounts are in rotation, of how many, and the moment
/// none are, which one returns first and when, in red.
///
/// It mirrors the 429 a client gets when nothing can serve it, so it counts the accounts that
/// client could get. A Claude request is served only by Claude accounts and a Codex request only
/// by Codex ones, and each provider's 429 counts its own, so a fleet holding both gets one
/// segment per provider (`CLAUDE 1/3 eligible │ CODEX 2/4 eligible`), each with its own count,
/// its own next-free account and its own alarm: one count over both would show a spent Claude
/// fleet green because a Codex account was ready. A fleet of one provider keeps one segment,
/// and a Claude-only fleet's is the `FLEET` banner it always had. See [`fleet_banner_line`].
pub(super) fn render_fleet_banner(
    frame: &mut Frame,
    area: Rect,
    snapshot: &StatsSnapshot,
    facts: &DashboardFacts,
    now: OffsetDateTime,
) {
    frame.render_widget(
        Paragraph::new(fleet_banner_line(snapshot, facts, now, area.width)),
        area,
    );
}

/// How much of its next-free tail a banner segment carries. The fitting steps shorten every
/// segment's tail together.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tail {
    /// ` · next free {name} in {rel}`.
    Named,
    /// ` · next free in {rel}`.
    Short,
    /// None at all.
    Dropped,
}

/// One segment's text: `{label} {ready}/{total} eligible`, and when none is ready its next-free
/// tail, `unknown` when no account has a known free time. A fleet with no Codex account has
/// one segment, labelled `FLEET`.
fn segment_text(label: &str, status: &FleetStatus, tail: Tail) -> String {
    let mut text = format!("{label} {}/{} eligible", status.eligible, status.total);
    if status.eligible == 0 {
        match (&status.next_free, tail) {
            (_, Tail::Dropped) => {}
            (Some((name, delta)), Tail::Named) => {
                text.push_str(&format!(" · next free {name} in {}", rel(*delta)));
            }
            (Some((_, delta)), Tail::Short) => {
                text.push_str(&format!(" · next free in {}", rel(*delta)));
            }
            (None, _) => text.push_str(" · next free unknown"),
        }
    }
    text
}

/// A segment's style: the alarm when none of its accounts is ready, a calm green otherwise.
fn segment_style(status: &FleetStatus) -> Style {
    if status.eligible == 0 {
        alarm_style()
    } else {
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD)
    }
}

/// Why codex is not pooled, in the banner's few words, keyed on the route's code: the decision
/// `tcr status` and `tcr run --codex` word in full. `None` when it is pooled, and for a code the
/// in-process route never gives (it is this process, so it always answers, with its own
/// listener, and the dashboard asks only while it holds a Codex account).
fn not_pooled_reason(route: &CodexRoute, ca_path: Option<&std::path::Path>) -> Option<String> {
    if route.pooled {
        return None;
    }
    Some(match route.code.as_deref()? {
        crate::codex_route::MITM_OFF => "MITM off (TLS material failed to load)".to_string(),
        crate::codex_route::LEAF_MISSING_HOST => "leaf does not cover chatgpt.com".to_string(),
        crate::codex_route::NO_CA => match ca_path {
            Some(ca) => format!("no readable CA at {}", ca.display()),
            // The route's own reason is the same sentence, with the path it checked.
            None => route.reason.clone()?,
        },
        crate::codex_route::UNKNOWN_LEAF => "leaf state unknown".to_string(),
        _ => return None,
    })
}

/// The fleet banner as one line, fitted to `width` chars. See [`render_fleet_banner`].
///
/// A fleet with no Codex account draws the one `FLEET` segment it always had, whatever the width:
/// the fitting below never runs for it. An empty fleet with a codex login on disk reads `FLEET
/// 0/0 eligible`, dim, since nothing is down; there is nothing yet.
///
/// A fleet holding Codex accounts gets a segment per provider, and the CODEX segment carries what
/// tcr knows about codex, each only while it holds: why codex is not pooled, how many Codex
/// requests tcr refused in the last ten minutes, and how many it relayed unpooled. When that does
/// not fit, the least urgent words go first: the unpooled count, then the refused count, then
/// the names in every next-free tail, then the tails. Last, the not-pooled reason is cut, with a
/// visible `…`, to the room left. The counts and ` · not pooled: ` always stay.
pub(super) fn fleet_banner_line(
    snapshot: &StatsSnapshot,
    facts: &DashboardFacts,
    now: OffsetDateTime,
    width: u16,
) -> Line<'static> {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let yellow = Style::default().fg(Color::Yellow);
    if !holds(snapshot, Provider::Codex) {
        if snapshot.accounts.is_empty() && facts.codex_login.is_some() {
            return Line::from(Span::styled("FLEET 0/0 eligible", dim));
        }
        let status = fleet_status(&snapshot.accounts, now);
        return Line::from(Span::styled(
            segment_text("FLEET", &status, Tail::Named),
            segment_style(&status),
        ));
    }

    let of = |provider: Provider| {
        fleet_status_of(
            snapshot.accounts.iter().filter(|a| a.provider == provider),
            now,
        )
    };
    let claude = holds(snapshot, Provider::Anthropic).then(|| of(Provider::Anthropic));
    let codex = of(Provider::Codex);
    let reason = facts
        .codex_route
        .as_ref()
        .and_then(|route| not_pooled_reason(route, facts.ca_path.as_deref()));
    let refused = facts.codex_traffic.as_ref().map_or(0, |t| t.refused_10m);
    let unpooled = facts.codex_traffic.as_ref().map_or(0, |t| t.unpooled_10m);
    let spans = |tail: Tail, with_refused: bool, with_unpooled: bool, reason: Option<&str>| {
        let mut spans = Vec::new();
        if let Some(claude) = &claude {
            spans.push(Span::styled(
                segment_text("CLAUDE", claude, tail),
                segment_style(claude),
            ));
            spans.push(Span::styled(" │ ", dim));
        }
        spans.push(Span::styled(
            segment_text("CODEX", &codex, tail),
            segment_style(&codex),
        ));
        if let Some(reason) = reason {
            spans.push(Span::styled(format!(" · not pooled: {reason}"), yellow));
        }
        if with_refused && refused > 0 {
            spans.push(Span::styled(format!(" · {refused} refused in 10m"), yellow));
        }
        if with_unpooled && unpooled > 0 {
            spans.push(Span::styled(format!(" · {unpooled} unpooled in 10m"), dim));
        }
        spans
    };
    let chars = |spans: &[Span]| -> usize { spans.iter().map(|s| s.content.chars().count()).sum() };
    let width = usize::from(width);
    for (tail, with_refused, with_unpooled) in [
        (Tail::Named, true, true),
        (Tail::Named, true, false),
        (Tail::Named, false, false),
        (Tail::Short, false, false),
        (Tail::Dropped, false, false),
    ] {
        let line = spans(tail, with_refused, with_unpooled, reason.as_deref());
        if chars(&line) <= width {
            return Line::from(line);
        }
    }
    match reason {
        Some(reason) => {
            let room = width.saturating_sub(chars(&spans(Tail::Dropped, false, false, Some(""))));
            Line::from(spans(
                Tail::Dropped,
                false,
                false,
                Some(&truncate(&reason, room)),
            ))
        }
        None => Line::from(spans(Tail::Dropped, false, false, None)),
    }
}

/// The one style for "this needs you NOW", shared by the two rows that raise an
/// alarm: the fleet banner with nothing in rotation, and any notice. Shared so the
/// two cannot drift apart — a notice that stopped looking like the banner would
/// read as ordinary chrome, which is exactly what it is not.
pub(super) fn alarm_style() -> Style {
    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
}
