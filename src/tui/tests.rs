//! The dashboard's tests, one file per part they test under `tests/`, and the snapshot and
//! buffer helpers those files share.

use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use tcr_status_wire::{CodexRoute, CodexTraffic};
use time::Duration as TimeDuration;

use crate::manager::DisablePersist;
use crate::probe::ProbeStatus;
use crate::provider::Provider;
use crate::stats::{
    AccountSnapshot, GateReason, QuotaState, SessionKind, SessionSnapshot, StatsSnapshot,
};

use super::*;
use super::{
    accounts::*, banner::*, cells::*, facts::*, frame::*, input::*, sessions::*, terminal::*,
};

mod accounts;
mod banner;
mod cells;
mod golden;
mod layout;
mod selection;
mod sessions;
mod terminal;

/// A fixed instant `secs` after the epoch — larger `secs` is younger, so the
/// group's youngest `last_seen` is the max of these.
fn at(secs: i64) -> OffsetDateTime {
    OffsetDateTime::UNIX_EPOCH + TimeDuration::seconds(secs)
}

/// A session sitting on its pin — pinned to and last served by `account`.
fn snap(id: &str, account: &str, requests: u64, last_seen: Option<i64>) -> SessionSnapshot {
    SessionSnapshot {
        id: id.to_string(),
        account: account.to_string(),
        last_served_account: account.to_string(),
        requests,
        last_seen: last_seen.map(at),
        provider: Provider::Anthropic,
        kind: SessionKind::Stable,
    }
}

/// A session PINNED to `account` whose most recent request was DIVERTED to
/// `served_by` — the pin never moved.
fn diverted(
    id: &str,
    account: &str,
    served_by: &str,
    requests: u64,
    last_seen: Option<i64>,
) -> SessionSnapshot {
    SessionSnapshot {
        last_served_account: served_by.to_string(),
        ..snap(id, account, requests, last_seen)
    }
}

/// A minimal account snapshot carrying just the fields the gate chip and fleet
/// banner read — everything else defaulted — for those render tests.
fn snap_gate(name: &str, gate: GateReason, free_at: Option<OffsetDateTime>) -> AccountSnapshot {
    AccountSnapshot {
        slot: 0,
        name: name.to_string(),
        provider: Provider::Anthropic,
        organization_type: None,
        rate_limit_tier: None,
        seat_tier: None,
        plan_display_name: None,
        org_uuid: None,
        org_name: None,
        priority: 0,
        status: "active".to_string(),
        disabled: matches!(gate, GateReason::Disabled),
        five_hour: None,
        five_hour_reset: None,
        five_hour_absent_on_plan: false,
        seven_day: None,
        seven_day_reset: None,
        seven_day_oi: None,
        seven_day_oi_reset: None,
        requests: 0,
        input_tokens: 0,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
        last_used: None,
        rate_limited_until: None,
        probe_status: ProbeStatus::Never,
        last_probe: None,
        quota_updated_at: None,
        probe_error: None,
        quota_state: QuotaState::Normal,
        gate,
        free_at,
        stream_error_count: 0,
        last_stream_error: None,
        groups: Vec::new(),
        reserved_groups: Vec::new(),
        parked_groups: Vec::new(),
        control_allowed_groups: Vec::new(),
        usage: None,
        codex: Default::default(),
    }
}

/// A stable, far-from-epoch anchor so `now + delta` never underflows.
fn anchor() -> OffsetDateTime {
    OffsetDateTime::UNIX_EPOCH + TimeDuration::days(3650)
}

/// Flatten a rendered buffer into one string per row so a render test can
/// assert on the visible text without caring about cell geometry.
fn buffer_rows(buffer: &Buffer) -> Vec<String> {
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect()
}

/// A one-account snapshot with learned quota buckets (5h 47%, 7d 90% at
/// `state`, Fable 62%) for the render tests — adapts [`snap_gate`], whose
/// buckets are all `None`.
fn util_snapshot(state: QuotaState) -> StatsSnapshot {
    let mut account = snap_gate("acct", GateReason::Ok, None);
    account.five_hour = Some(0.47);
    account.seven_day = Some(0.9);
    account.seven_day_oi = Some(0.62);
    account.quota_state = state;
    StatsSnapshot {
        accounts: vec![account],
        current: Some(0),
        current_by_provider: crate::stats::CurrentByProvider::default(),
        recent: vec![],
        sessions: vec![],
        wire_sessions: vec![],
        wire_sessions_summary: tcr_status_wire::SessionsSummary::default(),
    }
}

/// [`snap_gate`] as a Codex account.
fn codex_gate(name: &str, gate: GateReason, free_at: Option<OffsetDateTime>) -> AccountSnapshot {
    AccountSnapshot {
        provider: Provider::Codex,
        ..snap_gate(name, gate, free_at)
    }
}

/// A snapshot of `accounts`, each row's slot its position (no row was deleted), with no
/// account serving, no sessions and no log.
fn snapshot_of(accounts: Vec<AccountSnapshot>) -> StatsSnapshot {
    StatsSnapshot {
        accounts: accounts
            .into_iter()
            .enumerate()
            .map(|(slot, account)| AccountSnapshot { slot, ..account })
            .collect(),
        current: None,
        current_by_provider: crate::stats::CurrentByProvider::default(),
        recent: vec![],
        sessions: vec![],
        wire_sessions: vec![],
        wire_sessions_summary: tcr_status_wire::SessionsSummary::default(),
    }
}

/// Draw `snapshot`'s accounts pane at `width` x `height` with display row `selected` under
/// the cursor, and return the buffer.
fn accounts_buffer(snapshot: &StatsSnapshot, width: u16, height: u16, selected: usize) -> Buffer {
    let mut terminal =
        Terminal::new(TestBackend::new(width, height)).expect("test backend builds a terminal");
    terminal
        .draw(|frame| render_accounts(frame, frame.area(), snapshot, selected, anchor()))
        .expect("render succeeds");
    terminal.backend().buffer().clone()
}

/// The row of `rows` that holds `needle`, with its index, or a panic that shows the pane.
fn row_holding<'a>(rows: &'a [String], needle: &str) -> (usize, &'a str) {
    let Some((y, row)) = rows
        .iter()
        .enumerate()
        .find(|(_, row)| row.contains(needle))
    else {
        panic!("no row holds {needle:?}:\n{}", rows.join("\n"));
    };
    (y, row.as_str())
}

/// The x of `needle`'s first character in `row`, counted in cells.
fn cell_x(row: &str, needle: &str) -> u16 {
    let at = row
        .find(needle)
        .unwrap_or_else(|| panic!("{needle:?} is not in {row:?}"));
    row[..at].chars().count() as u16
}

/// The mixed fleet the section tests draw, in rotation order: alice (Claude, slot 0), bob
/// (Codex, 1), frank (Claude, 2), dana (Codex, 3). Interleaved, as a config that gained its
/// Codex accounts one at a time is.
fn interleaved_fleet() -> StatsSnapshot {
    snapshot_of(vec![
        snap_gate("alice@example.com", GateReason::Ok, None),
        codex_gate("bob@example.com/codex", GateReason::Ok, None),
        snap_gate("frank@example.com", GateReason::Ok, None),
        codex_gate("dana@example.com", GateReason::Ok, None),
    ])
}

/// One line of the recent-request log, served at `time`.
fn log_entry(
    status: u16,
    method: &str,
    path: &str,
    account: &str,
    time: OffsetDateTime,
) -> crate::stats::RequestLogEntry {
    crate::stats::RequestLogEntry {
        time,
        method: method.to_string(),
        path: path.to_string(),
        status,
        account: account.to_string(),
        provider: Provider::Anthropic,
    }
}
