//! The live ratatui dashboard: one row per account with quota bars, probe
//! health, and counters, plus a recent-request log. Repaints on a 500ms tick so
//! every quota bar is recomputed live against its reset — a window past its
//! reset can never render as still-full (the display half of bug #2).
//!
//! Terminal safety (behaviour #7): a [`TerminalGuard`] restores the terminal on
//! **any** exit path (normal, `?`, or unwind), and a panic hook restores it
//! before the default panic printer runs — so a crash never leaves the user in
//! raw-mode alt-screen. A single failed repaint is logged and swallowed rather
//! than crashing the loop, and paste / resize / focus events are non-fatal.

mod accounts;
mod banner;
mod cells;
mod facts;
mod frame;
mod input;
mod sessions;
mod terminal;
#[cfg(test)]
mod tests;

use std::io::{self};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

// Everything crossterm-shaped comes from `ratatui::crossterm` so it is the SAME
// copy ratatui's backend drives -- see the note on the `crossterm` dependency in
// Cargo.toml. `EventStream` is the one exception: it needs the `event-stream`
// feature, which ratatui does not pass through, so it comes from the direct dep.
// That split is deliberate -- it turns a version drift between the two into a
// compile error rather than a silently duplicated crate.
use crossterm::event::EventStream;
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::Event;
use time::OffsetDateTime;

use crate::manager::Manager;

pub(crate) use facts::DashboardFacts;
use facts::live_facts;
use frame::render_with;
use input::{
    Action, DrawnRow, Notice, drawn_rows, key_action, live_notice, next_selection, reanchor, toggle,
};
use terminal::{SCREEN_DIRTY, TerminalGuard, install_panic_hook, repaint};

/// Run the dashboard until the user quits (`q` or `Ctrl-C`). Returns once the
/// terminal has been restored by [`TerminalGuard`]'s drop.
pub async fn run(manager: Arc<Manager>) -> io::Result<()> {
    run_loop(manager, None).await
}

/// [`run`] with `facts` fixed for the whole run instead of read from this process each tick:
/// `tcr demo`, whose fake fleet must show none of this machine's MITM state, CA, codex login
/// or home folder.
pub(crate) async fn run_with_facts(manager: Arc<Manager>, facts: DashboardFacts) -> io::Result<()> {
    run_loop(manager, Some(facts)).await
}

async fn run_loop(manager: Arc<Manager>, fixed_facts: Option<DashboardFacts>) -> io::Result<()> {
    install_panic_hook();
    let _guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut events = EventStream::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(500));
    // A DISPLAY row of `drawn`, the table the last frame drew.
    let mut selected: usize = 0;
    let mut drawn: Vec<DrawnRow> = Vec::new();
    // The most recent "that keypress did not stick" warning, if it is still fresh.
    let mut notice: Option<Notice> = None;

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let now = OffsetDateTime::now_utc();
                let snapshot = manager.snapshot(now);
                let facts = match &fixed_facts {
                    Some(facts) => facts.clone(),
                    None => live_facts(&manager, &snapshot),
                };
                // The cursor follows its account into the table this frame draws, and a
                // key acts through that same table until the next frame replaces it.
                let next = drawn_rows(&snapshot);
                selected = reanchor(&drawn, selected, &next);
                drawn = next;
                let live = live_notice(
                    &notice,
                    std::time::Instant::now(),
                    drawn.get(selected).map(|row| row.name.as_str()),
                );
                repaint(&mut terminal, &SCREEN_DIRTY, |frame| {
                    render_with(frame, &snapshot, selected, live, &facts, now);
                });
            }
            maybe_event = events.next() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) => {
                        let action = key_action(&key);
                        // Clamp FIRST, move second — and both here, BEFORE the
                        // arms below read `selected`. The 500ms repaint used to be
                        // the only clamp, so a key acting on the selection could
                        // read an index `Down` had already pushed past the end.
                        // Against the table on screen, not the manager's count,
                        // which includes rows deleted from the config.
                        selected = next_selection(selected, drawn.len(), action);
                        match action {
                            Action::Quit => break,
                            // Moving off a row abandons whatever was said about it.
                            Action::Up | Action::Down => notice = None,
                            // A persist that did not reach disk is surfaced HERE,
                            // not only in the log the TUI has redirected away from
                            // the screen — otherwise the row renders benched and
                            // the user finds out at the next restart. The notice
                            // carries the name of the row it is about, so it cannot
                            // be read against a different one.
                            Action::Disable | Action::Enable => {
                                notice = toggle(
                                    &manager,
                                    &drawn,
                                    selected,
                                    action == Action::Disable,
                                );
                            }
                            // Asked for by hand: the user can see damage we could
                            // not detect. Consumed by the next tick.
                            Action::Redraw => SCREEN_DIRTY.store(true, Ordering::SeqCst),
                            Action::None => {}
                        }
                    }
                    // A resize repaints from a different geometry, with the old
                    // frame's cells still underneath. ratatui clears inside `draw`
                    // when it notices the new size, but that clear is a write like
                    // any other and can fail; marking the screen dirty puts the
                    // repair on the tick, which retries.
                    Some(Ok(Event::Resize(_, _))) => {
                        SCREEN_DIRTY.store(true, Ordering::SeqCst);
                    }
                    // Paste / focus / mouse are non-fatal; a multi-char paste can
                    // never crash the loop.
                    Some(Ok(_)) => {}
                    Some(Err(err)) => tracing::warn!(error = %err, "tui input error"),
                    None => break, // input stream closed
                }
            }
        }
    }
    Ok(())
}
