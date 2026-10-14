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

use std::collections::HashMap;
use std::io::{self};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

// Everything crossterm-shaped comes from `ratatui::crossterm` so it is the SAME
// copy ratatui's backend drives -- see the note on the `crossterm` dependency in
// Cargo.toml. `EventStream` is the one exception: it needs the `event-stream`
// feature, which ratatui does not pass through, so it comes from the direct dep.
// That split is deliberate -- it turns a version drift between the two into a
// compile error rather than a silently duplicated crate.
use crossterm::event::EventStream;
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use ratatui::{Frame, Terminal};
use tcr_status_wire::{CodexRoute, CodexTraffic};
use time::{Duration as TimeDuration, OffsetDateTime};

use crate::manager::{DisablePersist, Manager};
use crate::probe::ProbeStatus;
use crate::provider::Provider;
use crate::stats::{
    AccountSnapshot, GateReason, QuotaState, SessionKind, SessionSnapshot, StatsSnapshot,
};

/// Restores the terminal to a sane state whenever it is dropped — normal exit,
/// an early `?`, or a panic unwind. Constructing it enters raw-mode + the
/// alternate screen; dropping it leaves them, best-effort (a `Drop` never panics).
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableBracketedPaste);
    }
}

/// Set whenever the screen may no longer match what ratatui believes is on it.
/// The draw loop consumes it with a full clear and repaint.
///
/// ratatui writes only the cells that differ between its previous and current
/// buffers, so ANY divergence between that previous buffer and the real screen is
/// permanent: every later frame computes an empty diff for the damaged cells and
/// writes nothing at all. That is why issue #323 never recovered on its own and
/// why a restart was the only cure — an expensive one here, since every account
/// comes back with a cold prompt cache.
///
/// Three things open that gap, and all three set this flag:
///
/// - a panic on some OTHER thread, whose message lands in cells ratatui believes
///   it already owns (issue #323, first report);
/// - a `draw` that failed at its final `Backend::flush`. ratatui swaps its
///   buffers BEFORE that flush — `swap_buffers()` then `self.backend.flush()?` in
///   `ratatui_core::terminal::render::apply_buffer_with_cursor` — so a frame that
///   never left the process is recorded as though it had reached the screen
///   (issue #323, second report, on a build that already had the panic fix);
/// - a resize, which repaints from a different geometry over the old frame's
///   cells.
///
/// `r` sets it too. Whatever we failed to anticipate, the user gets the screen
/// back with one keystroke instead of a restart.
static SCREEN_DIRTY: AtomicBool = AtomicBool::new(false);

/// One repaint, with the screen repair that has to go with it.
///
/// Repairs first when something dirtied the screen, then draws — and treats a
/// failed draw as fresh damage rather than as nothing.
///
/// That last part is the whole point. Neither failure may crash the dashboard, and
/// neither may be forgotten. By the time a backend flush fails, ratatui has already
/// swapped its buffers (`swap_buffers()` precedes `self.backend.flush()?` in
/// `ratatui_core::terminal::render::apply_buffer_with_cursor`), so it now records a
/// frame that never left the process as the one on screen. Every later diff against
/// that record comes out empty, and the damaged cells stay damaged until something
/// clears. Re-arming `dirty` is what makes the next tick that something.
///
/// `dirty` is a parameter rather than a direct read of [`SCREEN_DIRTY`] so a test
/// can drive this with a flag of its own, instead of racing every other test in the
/// binary for one global.
fn repaint<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    dirty: &AtomicBool,
    render_frame: impl FnOnce(&mut Frame),
) {
    if dirty.swap(false, Ordering::SeqCst) {
        if let Err(err) = repair_screen(terminal) {
            // The repair did not land either. Stay dirty and retry on the next tick
            // rather than dropping it on the floor.
            tracing::warn!(error = %err, "could not clear a dirty screen");
            dirty.store(true, Ordering::SeqCst);
        }
    }
    if let Err(err) = terminal.draw(render_frame) {
        tracing::warn!(error = %err, "tui repaint failed; forcing a full redraw");
        dirty.store(true, Ordering::SeqCst);
    }
}

/// Clear the screen and force the next draw to repaint every cell, WITHOUT asking
/// the terminal where the cursor is.
///
/// `Terminal::clear` cannot be used here. Its first statement is
/// `self.backend.get_cursor_position()?` (`ratatui_core::terminal::buffers`), and
/// crossterm answers that by writing `ESC [ 6 n` to stdout and then waiting up to
/// 2000 ms to read the terminal's reply back off stdin
/// (`crossterm::cursor::sys::unix::read_position_raw`).
///
/// This TUI holds an `EventStream`, which is draining stdin the whole time. The
/// reply is delivered to whichever reader takes it first, so on a terminal where
/// the `EventStream` wins, `position()` never sees its answer, the clear fails
/// before clearing anything, and the tick blocks for two seconds losing it.
/// Reported on 1.1.3 against issue #323, once the screen-repair path this guards
/// actually started running:
///
/// ```text
/// WARN could not clear a dirty screen error=The cursor position could not be
///      read within a normal duration
/// ```
///
/// one line every two seconds, forever, because the retry re-armed a repair that
/// could never succeed.
///
/// `Terminal::resize` reaches the same end state by the path that does not ask.
/// For a fullscreen viewport it clears the whole region and resets the back
/// buffer, so the next diff is against an empty frame and every cell is rewritten
/// — and it reads the cursor only for an inline viewport, which this is not.
/// `Backend::size` is an ioctl, not a terminal round trip.
fn repair_screen<B: ratatui::backend::Backend>(terminal: &mut Terminal<B>) -> Result<(), B::Error> {
    let area = terminal.size()?.into();
    terminal.resize(area)
}

/// Does a panic on this thread mean the process is going down, so the terminal
/// must be handed back?
///
/// Only the thread running the TUI — `main`, under `#[tokio::main]` — takes the
/// process with it. A panicking background task does not: tokio catches it, the
/// task dies alone, and the dashboard keeps running.
///
/// Issue #323 is what the always-restore answer costs. A background task panicked,
/// the hook left the alternate screen, the TUI kept drawing, and ratatui's diff
/// repainted only the cells it believed had changed — so its output landed as
/// isolated characters scattered across whatever was on the screen, and no later
/// frame could repair it because ratatui thought every other cell was already
/// right. Reproduced at the reporter's 154x31 with `examples/tui_garble_repro.rs`:
/// the panic message came back as `a background task die15` and
/// `RUST_15CKTRACE`, single characters replaced by digits from another widget,
/// which is the report almost word for word.
fn panic_takes_the_process_down(thread_name: Option<&str>) -> bool {
    thread_name == Some("main")
}

/// Install a panic hook (once). A panic on the TUI's own thread restores the
/// terminal before the previous hook prints — otherwise a panic mid-render leaves
/// a corrupt screen. A panic anywhere else leaves the screen alone (see
/// [`panic_takes_the_process_down`]), records it in the log the TUI has redirected
/// to a file, and asks the draw loop for a full repaint.
fn install_panic_hook() {
    use std::sync::Once;
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let thread = std::thread::current();
            if panic_takes_the_process_down(thread.name()) {
                let _ = disable_raw_mode();
                let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableBracketedPaste);
                previous(info);
                return;
            }
            // Not ours to print. The default hook would write onto the dashboard.
            tracing::error!(
                thread = thread.name().unwrap_or("<unnamed>"),
                panic = %info,
                "a background task panicked"
            );
            SCREEN_DIRTY.store(true, Ordering::SeqCst);
        }));
    });
}

/// What a key event asks the loop to do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Action {
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
const NOTICE_SECONDS: u64 = 6;

/// A short-lived line above the accounts table, for the one thing the dashboard
/// otherwise cannot tell you: that the key you just pressed did not do what the
/// table now shows. `tracing` is redirected to a log file in TUI mode, so a failed
/// `disabled` write was reported only somewhere the user is not looking, while the
/// row happily rendered as benched.
struct Notice {
    text: &'static str,
    expires_at: std::time::Instant,
    /// The account the notice is ABOUT, by name — `None` when the keypress named
    /// no account at all. Every notice describes one row, so it must not be read
    /// against another: a warning raised for `alice` still on screen while `bob` is
    /// selected reads as a warning about `bob`.
    account: Option<String>,
}

impl Notice {
    fn new(text: &'static str, account: Option<String>) -> Self {
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
fn live_notice<'a>(
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
fn next_selection(selected: usize, count: usize, action: Action) -> usize {
    let selected = clamp_selected(selected, count);
    match action {
        Action::Up => selected.saturating_sub(1),
        Action::Down => selected.saturating_add(1),
        Action::None | Action::Quit | Action::Disable | Action::Enable | Action::Redraw => selected,
    }
}

/// One row of the accounts pane as it was drawn: the account's slot, the index every key acts
/// through ([`crate::stats::AccountSnapshot::slot`]), its name, which the notices name, and its
/// provider, which words them.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DrawnRow {
    idx: usize,
    name: String,
    provider: Provider,
}

/// The order the accounts pane draws a fleet in, as positions in its list: the Claude rows,
/// then the Codex rows, each in rotation order. A fleet of one provider is drawn in rotation
/// order, so nothing moves for it. The manager's own vector is never reordered: its indices are
/// what every pin, session and key holds.
fn display_order_of(providers: &[Provider]) -> Vec<usize> {
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
fn display_order(snapshot: &StatsSnapshot) -> Vec<usize> {
    let providers: Vec<Provider> = snapshot.accounts.iter().map(|a| a.provider).collect();
    display_order_of(&providers)
}

/// The position in the snapshot's list that display row `selected` draws, or `None` past the
/// last row.
fn key_target(order: &[usize], selected: usize) -> Option<usize> {
    order.get(selected).copied()
}

/// The table every key acts through: the rows of `snapshot`, in the order [`render_accounts_with`]
/// draws them ([`display_order`]), each naming its account by slot. Built from the same order
/// the pane is drawn from, and kept with the frame it was drawn for, so a key always acts on
/// the row the person sees under the cursor.
fn drawn_rows(snapshot: &StatsSnapshot) -> Vec<DrawnRow> {
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
fn reanchor(drawn: &[DrawnRow], selected: usize, next: &[DrawnRow]) -> usize {
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
fn toggle(
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
fn toggle_notice(
    persist: DisablePersist,
    disabled: bool,
    row: Option<&DrawnRow>,
) -> Option<Notice> {
    let provider = row.map_or(Provider::Anthropic, |row| row.provider);
    persist
        .warning_for(disabled, provider)
        .map(|text| Notice::new(text, row.map(|row| row.name.clone())))
}

/// What the dashboard knows beyond the snapshot, filled once per tick by the run loop
/// ([`dashboard_facts`]) so drawing never touches the filesystem or the clock itself. Every
/// field is `None` unless the fleet calls for it, so a Claude-only dashboard reads nothing new.
#[derive(Debug, Clone, Default)]
pub(crate) struct DashboardFacts {
    /// Whether this process pools codex, and why not: the decision `tcr status` and
    /// `tcr run --codex` take too ([`crate::codex_route`]). Only while the fleet holds a Codex
    /// row.
    pub(crate) codex_route: Option<CodexRoute>,
    /// What this process did with Codex requests in the last ten minutes. Only while the fleet
    /// holds a Codex row.
    pub(crate) codex_traffic: Option<CodexTraffic>,
    /// tcr's CA, which the not-pooled `no-ca` reason names. With [`Self::codex_route`].
    pub(crate) ca_path: Option<PathBuf>,
    /// The codex CLI's own login file, when one is on disk: found by its existence alone, never
    /// read. Only for an empty fleet, whose pane points at it.
    pub(crate) codex_login: Option<PathBuf>,
    /// The home folder, which a path a person reads shows as `~`.
    pub(crate) home: Option<PathBuf>,
}

/// The facts for `snapshot`, each asked for only when the fleet calls for it, as every Codex
/// surface is gated: `route` (with the number of Codex rows) and `traffic` only when a Codex row
/// exists, and `codex_login` only on an empty fleet. So a Claude-only fleet's dashboard stats
/// no file and computes no route.
fn dashboard_facts(
    snapshot: &StatsSnapshot,
    home: Option<PathBuf>,
    route: impl FnOnce(usize) -> (Option<CodexRoute>, Option<PathBuf>),
    traffic: impl FnOnce() -> Option<CodexTraffic>,
    codex_login: impl FnOnce() -> Option<PathBuf>,
) -> DashboardFacts {
    let codex_rows = snapshot
        .accounts
        .iter()
        .filter(|a| a.provider == Provider::Codex)
        .count();
    let (codex_route, ca_path) = if codex_rows > 0 {
        route(codex_rows)
    } else {
        (None, None)
    };
    DashboardFacts {
        codex_route,
        codex_traffic: if codex_rows > 0 { traffic() } else { None },
        ca_path,
        codex_login: if snapshot.accounts.is_empty() {
            codex_login()
        } else {
            None
        },
        home,
    }
}

/// [`dashboard_facts`] for the serving process: its own listener's report, through the one
/// in-process route decision (`codex_route::codex_route_in_process`), the CA and whether it is
/// readable as `tcr status` checks it, its own refusal rings, and codex's login file by its
/// existence alone.
fn live_facts(manager: &Manager, snapshot: &StatsSnapshot) -> DashboardFacts {
    dashboard_facts(
        snapshot,
        std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .map(PathBuf::from),
        |codex_rows| {
            let ca = crate::mitm::minted_ca_path();
            let ca_readable = ca.is_file();
            let route = crate::codex_route::codex_route_in_process(
                &manager.mitm_status(),
                manager.proxy_port(),
                &ca,
                ca_readable,
                codex_rows,
                manager.upstream_for(Provider::Codex),
            );
            (Some(route), Some(ca))
        },
        || Some(manager.codex_traffic(crate::now_ms())),
        || {
            let login = crate::codex_local::codex_login_path();
            crate::codex_local::login_file_exists(&login).then_some(login)
        },
    )
}

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

/// Map a key press to an [`Action`]. Key-release events (kitty/Windows) are
/// ignored so a keypress does not fire twice.
fn key_action(key: &KeyEvent) -> Action {
    if key.kind == KeyEventKind::Release {
        return Action::None;
    }
    match key.code {
        KeyCode::Char('q') | KeyCode::Char('Q') => Action::Quit,
        KeyCode::Char('c') | KeyCode::Char('C')
            if key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            Action::Quit
        }
        // `r` for redraw, plus the Ctrl-L every other full-screen program answers
        // to. Either one repairs a screen some other writer has corrupted.
        KeyCode::Char('l') | KeyCode::Char('L')
            if key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            Action::Redraw
        }
        KeyCode::Char('r') | KeyCode::Char('R') => Action::Redraw,
        KeyCode::Up | KeyCode::Char('k') => Action::Up,
        KeyCode::Down | KeyCode::Char('j') => Action::Down,
        KeyCode::Char('d') | KeyCode::Char('D') => Action::Disable,
        KeyCode::Char('e') | KeyCode::Char('E') => Action::Enable,
        _ => Action::None,
    }
}

/// Keep the sessions pane usable even when the terminal is short: the accounts
/// panel never eats so much height that fewer than this many rows remain for the
/// rest of the frame.
const SESSIONS_MIN: u16 = 3;

/// Full height (rows) the accounts panel wants: one row per account + a header
/// row + top/bottom borders.
const ACCOUNTS_CHROME: u16 = 3;

/// Height (rows) the accounts panel should get. It takes its full height whenever
/// at least `SESSIONS_MIN` rows remain for the rest of the frame; when the terminal
/// is too short for that, it yields down to `total_height - SESSIONS_MIN` (so the
/// recent-log shrinks/vanishes entirely before a single account row is dropped),
/// but never below a 4-row floor so a tiny terminal still shows the header plus at
/// least one account.
fn account_area_height(total_height: u16, n_accounts: u16) -> u16 {
    let acct_full = n_accounts.saturating_add(ACCOUNTS_CHROME);
    acct_full
        .min(total_height.saturating_sub(SESSIONS_MIN))
        .max(4)
}

/// The accounts panel title: honest about how many accounts are actually on
/// screen. `" teamclaude-rs · accounts (7) "` when all fit, or
/// `" teamclaude-rs · accounts (5/7 ▼) "` when rows are clipped.
fn accounts_title(shown: u16, total: u16) -> String {
    if shown >= total {
        format!(" teamclaude-rs · accounts ({total}) ")
    } else {
        format!(" teamclaude-rs · accounts ({shown}/{total} ▼) ")
    }
}

/// Paint the whole frame: accounts table on top, sessions pane in the middle,
/// request log below. No facts beyond the snapshot, at the clock's now: see
/// [`render_with`], which the run loop draws through with the facts it read.
#[cfg(test)]
fn render(frame: &mut Frame, snapshot: &StatsSnapshot, selected: usize, notice: Option<&str>) {
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
fn render_with(
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

/// Does the fleet hold an account of `provider`, in any state?
fn holds(snapshot: &StatsSnapshot, provider: Provider) -> bool {
    snapshot.accounts.iter().any(|a| a.provider == provider)
}

/// One row per account is the wrong altitude for "is the whole fleet up?" — the
/// live incident showed seven individually-busy rows and no aggregate. This
/// distilled fleet status is exactly that: how many accounts are in rotation, of
/// how many, and — only when NONE are — which account returns first and when.
struct FleetStatus {
    eligible: usize,
    total: usize,
    /// The soonest-returning account (name + time-until), computed ONLY when
    /// `eligible == 0`. `None` there means every gated account has an unknown
    /// clear-instant, so the banner says "next free unknown" rather than lie.
    next_free: Option<(String, TimeDuration)>,
}

/// Distil the account snapshots into a [`FleetStatus`]. An account is eligible
/// when its gate is [`GateReason::Ok`] and it is not disabled — the same hard
/// gates selection honours. When none are, the soonest known `free_at` names the
/// first account to return. Pure and terminal-free so it can be unit-tested.
fn fleet_status(accounts: &[AccountSnapshot], now: OffsetDateTime) -> FleetStatus {
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
fn render_fleet_banner(
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
        crate::codex_route::JS_LEAF => "JS leaf covers api.anthropic.com only".to_string(),
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
fn fleet_banner_line(
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
fn alarm_style() -> Style {
    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
}

/// The transient notice row. Painted in the alarm style, because every notice we
/// raise means an action the user believes happened did not.
fn render_notice(frame: &mut Frame, area: Rect, text: &str) {
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(text.to_string(), alarm_style()))),
        area,
    );
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

/// Which of the two accounts-table layouts a given pane width gets. The choice
/// is a *deterministic breakpoint*, not the constraint solver's squeeze: ratatui
/// silently shrinks over-wide `Length` columns to make a too-wide table fit, and
/// the first casualty is the trailing `%` of each utilization bar — the exact
/// number Gil needs most on a small screen. An honest display degrades by
/// *choosing* what to drop (here the Probe/Cache columns, and the bars in favour
/// of bare percentages) rather than letting the solver clip a number silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AccountsLayout {
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
const FULL_COLUMNS: [&str; 14] = [
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
const FULL_COLUMN_WIDTHS: [u16; 14] = [18, 3, 9, 15, 11, 22, 27, 15, 6, 8, 7, 8, 6, 4];

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
const FULL_LAYOUT_MIN_WIDTH: u16 = 174;

/// Pick the accounts-table layout for a pane `width` — the pure, rendering-free
/// core of the responsive table, so the breakpoint is unit-testable without a
/// terminal. See [`AccountsLayout`] for *why* the breakpoint is deterministic.
fn accounts_layout(width: u16) -> AccountsLayout {
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
fn codex_name_cell(name: &str, short_plan: Option<&str>) -> (String, Option<String>) {
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
fn cut_name(name: &str, room: usize) -> String {
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
    account.provider == Provider::Codex
        && account
            .codex
            .quota_windows
            .as_ref()
            .is_some_and(|windows| !windows.iter().any(|w| w == window))
}

/// The cell of a usage window the plan does not have: `none`, dim. Not `—`, which keeps
/// meaning "not measured yet".
fn none_cell() -> Cell<'static> {
    Cell::from("none").style(Style::default().add_modifier(Modifier::DIM))
}

/// The accounts pane with no facts beyond the snapshot. See [`render_accounts_with`], which
/// [`render_with`] draws through.
#[cfg(test)]
fn render_accounts(
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

/// The rows a mixed pane spends on anything but accounts: its title border, the column
/// header, the divider between the two sections, and the bottom border.
const MIXED_ACCOUNTS_CHROME: u16 = ACCOUNTS_CHROME + 1;

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
fn render_accounts_with(
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
        .position(|&pos| snapshot.accounts[pos].provider == Provider::Codex)
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
            section_title("teamclaude-rs · accounts · codex", shown, total)
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
        under_cursor.is_some_and(|account| account.provider == Provider::Codex),
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
                "teamclaude-rs · accounts · claude",
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
    if account.provider != Provider::Codex || account.gate != GateReason::Login {
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
fn split_rows(available: u16, claude: u16, codex: u16, cursor_in_codex: bool) -> (u16, u16) {
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
    let codex = account.provider == Provider::Codex;
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
                if codex {
                    Cell::from("")
                } else {
                    Cell::from(bar(account.seven_day_oi))
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
            if codex {
                Cell::from("")
            } else {
                Cell::from(pct(account.seven_day_oi))
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

/// One row in the sessions pane's account→session tree: either an account
/// header carrying the group's aggregates, or one of that account's sessions.
enum TreeRow {
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
    /// when its key is `Some`, and `handle` (`src/proxy.rs`) never pairs
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
fn session_tree(sessions: &[SessionSnapshot]) -> Vec<TreeRow> {
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
fn render_sessions(frame: &mut Frame, area: Rect, snapshot: &StatsSnapshot, now: OffsetDateTime) {
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
            Cell::from("(no active sessions)").style(Style::default().fg(Color::DarkGray))
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
fn render_log(frame: &mut Frame, area: Rect, snapshot: &StatsSnapshot, now: OffsetDateTime) {
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

/// An 8-cell utilization bar plus a percentage, or a dash if never learned.
fn bar(util: Option<f64>) -> String {
    const WIDTH: usize = 8;
    match util {
        None => format!("[{}]   —", "·".repeat(WIDTH)),
        Some(util) => {
            let clamped = util.clamp(0.0, 1.0);
            let filled = (clamped * WIDTH as f64).round() as usize;
            let filled = filled.min(WIDTH);
            let mut bar = String::with_capacity(WIDTH + 8);
            bar.push('[');
            for cell in 0..WIDTH {
                bar.push(if cell < filled { '#' } else { '·' });
            }
            bar.push(']');
            bar.push_str(&format!(" {:>3}%", (util * 100.0).round() as i64));
            bar
        }
    }
}

/// [`bar`] plus a right-aligned "time until reset" suffix — the 5h/7d cells in
/// [`AccountsLayout::Full`] only (`Compact` drops the countdown, see
/// [`AccountsLayout`]). `-` when `reset` was never learned or has already
/// elapsed, never a negative countdown. Uses [`rel`] as-is rather than a wider
/// day+hour form: `rel` is shared with the gate chip and the fleet banner, and
/// widening its multi-day output would silently change both of those too.
fn bar_with_reset(util: Option<f64>, reset: Option<OffsetDateTime>, now: OffsetDateTime) -> String {
    let countdown = match reset {
        Some(reset) if reset > now => rel(reset - now),
        _ => "-".to_string(),
    };
    format!("{} {countdown:>6}", bar(util))
}

/// The bar-less quota cell used in [`AccountsLayout::Compact`]: the SAME
/// percentage [`bar`] prints, with the 8-cell bar dropped so the number itself
/// is the whole cell. A plain right-aligned `%` cannot lose its digits to the
/// constraint solver the way a bar's trailing `%` does — which is the entire
/// point of compact mode. `—` mirrors [`bar`]'s never-learned dash so an
/// un-probed bucket reads identically in both layouts.
fn pct(util: Option<f64>) -> String {
    match util {
        None => "—".to_string(),
        Some(util) => format!("{:>3}%", (util * 100.0).round() as i64),
    }
}

/// The probe-health cell: an age since the last probe plus a coloured status.
fn probe_cell(account: &AccountSnapshot, now: OffsetDateTime) -> (String, Style) {
    let age = fmt_age_opt(account.last_probe, now);
    match account.probe_status {
        ProbeStatus::Ok => (format!("ok {age}"), Style::default().fg(Color::Green)),
        ProbeStatus::Error => (format!("ERR {age}"), Style::default().fg(Color::Red)),
        ProbeStatus::Timeout => (format!("T/O {age}"), Style::default().fg(Color::Red)),
        // Endpoint busy/throttled (usage-endpoint 429 or a transient upstream 5xx)
        // — benign, not a serving failure. Yellow, never red: the account's own
        // quota bar is still valid; only the probe was deflected.
        ProbeStatus::RateLimited => (format!("busy {age}"), Style::default().fg(Color::Yellow)),
        // A SUSTAINED run of 5xx, not a benign throttle — the usage endpoint
        // itself is down. Red, like `Error`: this is the visible state
        // `RateLimited`'s doc-comment says must not hide behind it forever.
        ProbeStatus::UpstreamDown => (format!("DOWN {age}"), Style::default().fg(Color::Red)),
        ProbeStatus::Never => ("never".to_string(), Style::default().fg(Color::DarkGray)),
    }
}

/// The weekly-bar quota annotation: a short honest label plus a colour, driven
/// by how close the account is to its own threshold. Never red-for-error — a
/// quota-parked account is operationally active; only the utilization is high.
fn quota_cell(state: QuotaState) -> (&'static str, Style) {
    match state {
        QuotaState::Normal => ("", Style::default()),
        QuotaState::NearLimit => (" near", Style::default().fg(Color::Yellow)),
        QuotaState::Exhausted => (" full", Style::default().fg(Color::Red)),
    }
}

/// The per-row gate chip: WHY this account is out of rotation and WHEN it
/// returns, mirroring the [`GateReason`] the manager computed and formatting the
/// `free_at` clear-instant as a compact back-when. `OK`/`OFF` are dim (not a
/// problem); the gates that can show no countdown at all — a dead
/// credential's `LOGIN`, an upstream-`REJECTED` account, and on a Codex row a
/// `REFUSED` with no known end and an `UNREACHABLE` one — are red-bold; every
/// quota/hold gate is red. Red-bold is about the missing countdown and NOT about
/// needing a person: this line used to say "the gates only a human clears",
/// which is true of `LOGIN` and false of `REJECTED` (see the `REJECTED` arm
/// below, and `Quota::drop_rejection_if_a_window_rolled`). An
/// unknown clear-instant drops the back-when (a bare
/// `5H`) — the display never invents a time the manager could not promise.
///
/// A Codex row speaks ChatGPT's terms where they differ ([`codex_gate_chip`]); every
/// other gate it shares with a Claude row reads the same on both.
fn gate_chip(account: &AccountSnapshot, now: OffsetDateTime) -> (String, Style) {
    if account.provider == Provider::Codex {
        if let Some(chip) = codex_gate_chip(account, now) {
            return chip;
        }
    }
    let red = Style::default().fg(Color::Red);
    let dim = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::DIM);
    // A quota/Fable gate's back-when: the compact `rel`, or just the prefix when
    // the reset is unknown.
    let back = |prefix: &str| match account.free_at {
        Some(f) if f > now => format!("{prefix} {}", rel(f - now)),
        _ => prefix.to_string(),
    };
    match account.gate {
        GateReason::Ok => ("OK".to_string(), dim),
        GateReason::Hold => {
            // A hold is short (<= 1h), so raw seconds read best ("HOLD 12s").
            let label = match account.free_at {
                Some(f) if f > now => format!("HOLD {}s", (f - now).whole_seconds().max(1)),
                _ => "HOLD".to_string(),
            };
            (label, red)
        }
        // The short hold an upstream 403 arms: a countdown like HOLD, named so
        // the row says the credential was refused rather than rate-limited.
        GateReason::Refused => {
            let label = match account.free_at {
                Some(f) if f > now => format!("REFUSED {}s", (f - now).whole_seconds().max(1)),
                _ => "REFUSED".to_string(),
            };
            (label, red)
        }
        GateReason::FiveHour => (back("5H"), red),
        GateReason::SevenDay => (back("7D"), red),
        GateReason::FableWeekly => (back("FABLE-7D"), red),
        GateReason::Standard => (back("STD"), red),
        GateReason::Login => (
            "LOGIN".to_string(),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        // Anthropic's own verdict, read from `anthropic-ratelimit-unified-status`
        // (`Quota::update_from_headers`). Only a Claude row reaches this arm: a Codex
        // row's refusal is ChatGPT's, and `codex_gate_chip` draws it REFUSED. Red-bold
        // like LOGIN because it is not a countdown: no single instant is known at
        // which this account returns, which is why `account_gate` reports
        // `free_at = None` here.
        //
        // It is no longer permanent, though, and this comment twice said otherwise.
        // The trap was that `update_from_headers` needs a SERVED response while
        // `account_terminal_gate` skips a rejected account, so nothing ever re-asked
        // and the rejection outlived its own window. The background probe now closes
        // that loop in `Quota::drop_rejection_if_a_window_rolled`: a probed
        // utilization strictly below the stored one means upstream started a new
        // window, so the rejection recorded against the old one is dropped and the
        // account re-enters rotation on its next probe. Still red while it stands,
        // because until that evidence arrives there is nothing to count down to.
        //
        // The countdown now shown in this row's 5h and 7d cells is each WINDOW's own
        // reset. That is a fact about the window and makes no claim about when this
        // account returns, which is why `account_gate` still reports `free_at = None`
        // here.
        GateReason::Rejected => (
            "REJECTED".to_string(),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        GateReason::Disabled => ("OFF".to_string(), dim),
        // Cleared only by `tcr group unreserve`, never a timer — same red-bold
        // treatment as LOGIN/REJECTED, not the dim OFF of an operator-disabled
        // account (the credential itself is fine; the pool is deliberately
        // holding it back).
        GateReason::Reserved => (
            "RESERVED".to_string(),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        // An operator's own choice, exactly like `Disabled` above — one config
        // key holding the whole group back — so it wears the same dim treatment
        // rather than the red of a gate nobody chose.
        GateReason::Parked => ("PARKED".to_string(), dim),
    }
}

/// The chips a Codex row wears where ChatGPT's terms differ from Anthropic's, or `None` for a
/// gate both providers read alike (`OK`, `LOGIN`, `OFF`, `PARKED`, a plain rate-limit `HOLD`,
/// and an upstream 403's own short hold, `REFUSED {n}s`).
///
/// - **`REFUSED`** is ChatGPT refusing the account where no full window explains it (credits,
///   a spend cap): the `Rejected` gate. While a hold counts that refusal down (`Hold`, or
///   `Refused` when an upstream 403 armed the hold) the chip carries the time left, `REFUSED
///   {rel}`, in the timed-chip style, and `limit_reached_type` is what tells it from a plain
///   hold. Not REJECTED, Anthropic's word, because the two clear differently: ChatGPT's refusal
///   clears when a probe reads the account allowed, or sees one of its windows roll
///   (`Quota::apply_usage`); Anthropic's only on a probed window roll.
/// - **`UNREACHABLE`** is a Codex member of a reserved group: a reserved group serves only the
///   requests that ask for it, and a Codex request carries no group, so nothing reaches the
///   account. Red, not the dim of PARKED: the operator chose a reservation, not to pause it.
/// - **A window that ran out** is named by its length, as the plan reported it (`30D`, `5H`),
///   because a ChatGPT plan's windows are not always five hours and seven days. With no
///   reported length it reads `5H`/`7D`, as on a Claude row.
fn codex_gate_chip(account: &AccountSnapshot, now: OffsetDateTime) -> Option<(String, Style)> {
    let red = Style::default().fg(Color::Red);
    let red_bold = red.add_modifier(Modifier::BOLD);
    let back = |prefix: String| match account.free_at {
        Some(f) if f > now => format!("{prefix} {}", rel(f - now)),
        _ => prefix,
    };
    let refused_by_chatgpt = account.codex.limit_reached_type.is_some();
    Some(match account.gate {
        GateReason::Rejected => ("REFUSED".to_string(), red_bold),
        GateReason::Hold | GateReason::Refused if refused_by_chatgpt => match account.free_at {
            Some(f) if f > now => (format!("REFUSED {}", rel(f - now)), red),
            _ => ("REFUSED".to_string(), red_bold),
        },
        GateReason::Reserved => ("UNREACHABLE".to_string(), red_bold),
        GateReason::FiveHour => (
            back(window_label(account.codex.five_hour_window_minutes, "5H")),
            red,
        ),
        GateReason::SevenDay => (
            back(window_label(account.codex.seven_day_window_minutes, "7D")),
            red,
        ),
        _ => return None,
    })
}

/// A usage window's chip prefix from its length in minutes: whole days read `{n}D`, whole hours
/// `{n}H`, anything else `{n}M`. With no length reported, `fallback`.
fn window_label(minutes: Option<u32>, fallback: &str) -> String {
    const DAY: u32 = 24 * 60;
    match minutes {
        Some(m) if m > 0 && m % DAY == 0 => format!("{}D", m / DAY),
        Some(m) if m > 0 && m % 60 == 0 => format!("{}H", m / 60),
        Some(m) if m > 0 => format!("{m}M"),
        _ => fallback.to_string(),
    }
}

fn status_style(status: &str) -> Style {
    match status {
        "active" => Style::default().fg(Color::Green),
        "throttled" => Style::default().fg(Color::Yellow),
        "error" => Style::default().fg(Color::Red),
        _ => Style::default(),
    }
}

fn status_color(status: u16) -> Color {
    match status {
        200..=299 => Color::Green,
        429 => Color::Magenta,
        400..=499 => Color::Yellow,
        500..=599 => Color::Red,
        _ => Color::Gray,
    }
}

/// Humanize a token count: `1234` → `1.2k`, `2_000_000` → `2.0M`.
fn fmt_tokens(count: u64) -> String {
    if count >= 1_000_000 {
        format!("{:.1}M", count as f64 / 1_000_000.0)
    } else if count >= 1_000 {
        format!("{:.1}k", count as f64 / 1_000.0)
    } else {
        count.to_string()
    }
}

/// Format the prompt-cache hit ratio (`cache_read / input_total`) as a whole
/// percentage. R3 GATE: `input_total == 0` renders "-" (never NaN / no divide),
/// so a never-served account shows a dash rather than a bogus 0%.
fn fmt_cache_ratio(cache_read: u64, input_total: u64) -> String {
    if input_total == 0 {
        "-".to_string()
    } else {
        format!("{:.0}%", cache_read as f64 / input_total as f64 * 100.0)
    }
}

/// Age of an optional instant relative to `now`, or an em-dash when absent —
/// the "—"-or-[`fmt_age`] pattern shared by the account, session, and probe cells.
fn fmt_age_opt(seen: Option<OffsetDateTime>, now: OffsetDateTime) -> String {
    seen.map_or_else(|| "—".to_string(), |t| fmt_age(now - t))
}

/// Humanize a duration as a compact age (`45s`, `12m`, `3h`, `2d`).
fn fmt_age(delta: TimeDuration) -> String {
    let secs = delta.whole_seconds().max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3_600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3_600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

/// A compact "time until" for the fleet banner and gate chips: seconds under a
/// minute, whole minutes under an hour, `HhMMm` under two days, whole days
/// beyond. Distinct from [`fmt_age`] (a single-unit ELAPSED age): a gate's
/// back-when keeps hour+minute detail within a day so `2h05m` is not flattened
/// to `2h`.
fn rel(d: TimeDuration) -> String {
    let secs = d.whole_seconds().max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3_600 {
        format!("{}m", secs / 60)
    } else if secs < 172_800 {
        format!("{}h{:02}m", secs / 3_600, (secs % 3_600) / 60)
    } else {
        format!("{}d", secs / 86_400)
    }
}

/// Truncate a string to `max` chars, appending `…` when it was cut.
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let head: String = text.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

#[cfg(test)]
mod panic_policy_tests {
    use super::panic_takes_the_process_down;

    /// The TUI's own thread. A panic here ends the process, so the terminal must be
    /// handed back or the user's shell is left in raw mode on the alternate screen.
    #[test]
    fn a_panic_on_the_tui_thread_restores_the_terminal() {
        assert!(panic_takes_the_process_down(Some("main")));
    }

    /// Every other thread. Issue #323: a background task's panic used to drop the
    /// alternate screen while the dashboard kept drawing, and ratatui's diff then
    /// painted single characters over whatever was on the screen forever.
    #[test]
    fn a_panic_on_any_other_thread_leaves_the_screen_alone() {
        for name in [
            Some("tokio-runtime-worker"),
            Some("tokio-rt-work15"),
            Some("probe-loop"),
            None,
        ] {
            assert!(
                !panic_takes_the_process_down(name),
                "{name:?} does not end the process, so it must not take the screen"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    use super::*;

    #[test]
    fn account_area_height_full_when_tall() {
        // Plenty of room: accounts gets its full height (7 rows + header + 2 borders).
        assert_eq!(account_area_height(40, 7), 10);
    }

    #[test]
    fn account_area_height_yields_log_before_clipping_accounts() {
        // Fits accounts (10) + SESSIONS_MIN (3) = 13 but not the full 9-row log:
        // the log shrank, every account is still shown.
        assert_eq!(account_area_height(13, 7), 10);
    }

    #[test]
    fn account_area_height_clips_only_when_forced() {
        // Genuinely tiny: accounts yields to total - SESSIONS_MIN, but never below 4.
        assert_eq!(account_area_height(8, 7), 5);
        assert!(account_area_height(8, 7) >= 4);
        assert_eq!(account_area_height(4, 7), 4);
    }

    #[test]
    fn accounts_title_shows_total_when_all_visible() {
        assert_eq!(accounts_title(7, 7), " teamclaude-rs · accounts (7) ");
    }

    #[test]
    fn accounts_title_flags_clip() {
        assert_eq!(accounts_title(5, 7), " teamclaude-rs · accounts (5/7 ▼) ");
    }

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

    /// A backend that models a buffered writer over a real tty: cells handed to
    /// `draw` sit in a pending queue and only reach the screen when `flush`
    /// succeeds. A failing flush drops them, exactly as a `BufWriter` over a tty
    /// that returns `EAGAIN` does.
    #[derive(Debug)]
    struct FlushFailure;
    impl std::fmt::Display for FlushFailure {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "flush failed")
        }
    }
    impl std::error::Error for FlushFailure {}

    struct PendingBackend {
        width: u16,
        height: u16,
        screen: Vec<Vec<char>>,
        pending: Vec<(u16, u16, char)>,
        fail_flush: bool,
        fail_clear: bool,
        /// Models a terminal whose `ESC [ 6 n` reply never comes back, because
        /// something else (here, the real TUI's `EventStream`) drained stdin first.
        fail_cursor_read: bool,
        cursor_reads: usize,
    }

    impl PendingBackend {
        fn new(width: u16, height: u16) -> Self {
            Self {
                width,
                height,
                screen: vec![vec![' '; width as usize]; height as usize],
                pending: Vec::new(),
                fail_flush: false,
                fail_clear: false,
                fail_cursor_read: false,
                cursor_reads: 0,
            }
        }
        fn row(&self, y: usize) -> String {
            self.screen[y]
                .iter()
                .collect::<String>()
                .trim_end()
                .to_string()
        }
    }

    impl ratatui::backend::Backend for PendingBackend {
        type Error = FlushFailure;
        fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
        where
            I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
        {
            for (x, y, cell) in content {
                self.pending
                    .push((x, y, cell.symbol().chars().next().unwrap_or(' ')));
            }
            Ok(())
        }
        fn hide_cursor(&mut self) -> Result<(), Self::Error> {
            Ok(())
        }
        fn show_cursor(&mut self) -> Result<(), Self::Error> {
            Ok(())
        }
        fn get_cursor_position(&mut self) -> Result<ratatui::layout::Position, Self::Error> {
            self.cursor_reads += 1;
            if self.fail_cursor_read {
                return Err(FlushFailure);
            }
            Ok(ratatui::layout::Position::new(0, 0))
        }
        fn set_cursor_position<P: Into<ratatui::layout::Position>>(
            &mut self,
            _position: P,
        ) -> Result<(), Self::Error> {
            Ok(())
        }
        fn clear(&mut self) -> Result<(), Self::Error> {
            if self.fail_clear {
                return Err(FlushFailure);
            }
            self.screen = vec![vec![' '; self.width as usize]; self.height as usize];
            Ok(())
        }
        fn clear_region(
            &mut self,
            _clear_type: ratatui::backend::ClearType,
        ) -> Result<(), Self::Error> {
            ratatui::backend::Backend::clear(self)
        }
        fn size(&self) -> Result<ratatui::layout::Size, Self::Error> {
            Ok(ratatui::layout::Size::new(self.width, self.height))
        }
        fn window_size(&mut self) -> Result<ratatui::backend::WindowSize, Self::Error> {
            Ok(ratatui::backend::WindowSize {
                columns_rows: ratatui::layout::Size::new(self.width, self.height),
                pixels: ratatui::layout::Size::new(0, 0),
            })
        }
        fn flush(&mut self) -> Result<(), Self::Error> {
            if self.fail_flush {
                // The bytes never left the process.
                self.pending.clear();
                return Err(FlushFailure);
            }
            for (x, y, ch) in self.pending.drain(..) {
                if (y as usize) < self.screen.len() && (x as usize) < self.screen[0].len() {
                    self.screen[y as usize][x as usize] = ch;
                }
            }
            Ok(())
        }
    }

    fn paint(
        terminal: &mut Terminal<PendingBackend>,
        text: &'static str,
    ) -> Result<(), FlushFailure> {
        terminal.draw(|frame| {
            let area = frame.area();
            frame.render_widget(Paragraph::new(text), area);
        })?;
        Ok(())
    }

    /// Why a failed repaint MUST mark the screen dirty (issue #323, second report).
    ///
    /// ratatui calls `swap_buffers()` before the final `Backend::flush`, so a frame
    /// that never reached the terminal is still recorded as the previous frame.
    /// Every later diff against that record is empty and nothing repaints — the
    /// reporter's "it never recovers, only a restart clears it", exactly.
    #[test]
    fn a_failed_flush_desyncs_ratatui_from_the_screen_forever() {
        let mut terminal = Terminal::new(PendingBackend::new(20, 1)).unwrap();
        paint(&mut terminal, "FRAME-ONE").unwrap();
        assert_eq!(terminal.backend().row(0), "FRAME-ONE");

        terminal.backend_mut().fail_flush = true;
        assert!(
            paint(&mut terminal, "FRAME-TWO").is_err(),
            "the failing flush must surface as an error"
        );
        terminal.backend_mut().fail_flush = false;

        // Four healthy frames, all rendering what the app wants on screen.
        for _ in 0..4 {
            paint(&mut terminal, "FRAME-TWO").unwrap();
        }
        assert_eq!(
            terminal.backend().row(0),
            "FRAME-ONE",
            "if this now recovers on its own, ratatui changed its swap/flush order              and the SCREEN_DIRTY handling in `run` can be simplified"
        );
    }

    /// THE gate for issue #323's second report: the draw loop itself recovers.
    ///
    /// Drives `repaint` — the real policy the tick runs — across six ticks with one
    /// failing flush in the middle, on a backend that drops writes exactly as a
    /// buffered writer over a tty does. Delete the `dirty.store(true, ..)` in
    /// `repaint`'s draw-error arm and this test fails with the screen frozen on the
    /// pre-failure frame, which is the bug as reported.
    #[test]
    fn repaint_recovers_the_screen_after_a_failed_flush() {
        let dirty = AtomicBool::new(false);
        let mut terminal = Terminal::new(PendingBackend::new(20, 1)).unwrap();

        let tick = |terminal: &mut Terminal<PendingBackend>, text: &'static str| {
            repaint(terminal, &dirty, |frame| {
                frame.render_widget(Paragraph::new(text), frame.area());
            });
        };

        tick(&mut terminal, "FRAME-ONE");
        assert_eq!(terminal.backend().row(0), "FRAME-ONE");

        // One tick's frame never leaves the process.
        terminal.backend_mut().fail_flush = true;
        tick(&mut terminal, "FRAME-TWO");
        terminal.backend_mut().fail_flush = false;
        assert_eq!(
            terminal.backend().row(0),
            "FRAME-ONE",
            "the failed tick must not have reached the screen — otherwise this test \
             is not modelling the failure it claims to"
        );

        // The very next tick repairs it.
        tick(&mut terminal, "FRAME-TWO");
        assert_eq!(
            terminal.backend().row(0),
            "FRAME-TWO",
            "the tick after a failed repaint must clear and fully redraw; without \
             that, ratatui diffs against a frame that never landed and the screen \
             stays broken until the process restarts (issue #323)"
        );
        assert!(
            !dirty.load(Ordering::SeqCst),
            "a successful repair must consume the flag, not leave the dashboard \
             clearing on every tick"
        );
    }

    /// The repair must not ask the terminal where the cursor is.
    ///
    /// `Terminal::clear` does, and crossterm answers by writing `ESC [ 6 n` and
    /// reading the reply off stdin — which this TUI's `EventStream` is already
    /// draining. On a terminal where the EventStream wins that race the read times
    /// out after two seconds and the clear fails having cleared nothing, so the
    /// repair re-arms and blocks the tick every time. That is issue #323's tail on
    /// 1.1.3: `could not clear a dirty screen error=The cursor position could not
    /// be read within a normal duration`, one line every two seconds, forever.
    ///
    /// Swap `repair_screen` back to `terminal.clear()` and this test fails.
    #[test]
    fn the_repair_never_reads_the_cursor_position() {
        let dirty = AtomicBool::new(false);
        let mut terminal = Terminal::new(PendingBackend::new(20, 1)).unwrap();

        // Put a frame up, then desync by losing one to a failed flush.
        repaint(&mut terminal, &dirty, |f| {
            f.render_widget(Paragraph::new("FRAME-ONE"), f.area());
        });
        terminal.backend_mut().fail_flush = true;
        repaint(&mut terminal, &dirty, |f| {
            f.render_widget(Paragraph::new("FRAME-TWO"), f.area());
        });
        terminal.backend_mut().fail_flush = false;
        assert!(
            dirty.load(Ordering::SeqCst),
            "the failed draw must arm the repair"
        );

        // From here the terminal will not answer a cursor query, like the reporter's.
        terminal.backend_mut().fail_cursor_read = true;
        let before = terminal.backend().cursor_reads;

        repaint(&mut terminal, &dirty, |f| {
            f.render_widget(Paragraph::new("FRAME-TWO"), f.area());
        });

        assert_eq!(
            terminal.backend().cursor_reads,
            before,
            "the repair asked the terminal for the cursor position; on a terminal \
             that cannot answer, that costs a 2s timeout per tick and repairs nothing"
        );
        assert_eq!(
            terminal.backend().row(0),
            "FRAME-TWO",
            "the repair must still have repainted the screen"
        );
        assert!(
            !dirty.load(Ordering::SeqCst),
            "a successful repair must consume the flag"
        );
    }

    /// A clear that fails too must stay armed, or the one tick that tried to repair
    /// the screen is also the tick that gave up on it.
    #[test]
    fn a_failed_clear_stays_armed_for_the_next_tick() {
        let dirty = AtomicBool::new(true);
        let mut terminal = Terminal::new(PendingBackend::new(20, 1)).unwrap();
        terminal.backend_mut().fail_clear = true;

        repaint(&mut terminal, &dirty, |frame| {
            frame.render_widget(Paragraph::new("X"), frame.area());
        });

        assert!(
            dirty.load(Ordering::SeqCst),
            "a clear that failed must leave the screen marked dirty"
        );
    }

    /// And why a clear is the repair: the same sequence, with the invalidation the
    /// draw loop now performs.
    #[test]
    fn a_clear_repairs_a_desynced_screen() {
        let mut terminal = Terminal::new(PendingBackend::new(20, 1)).unwrap();
        paint(&mut terminal, "FRAME-ONE").unwrap();

        terminal.backend_mut().fail_flush = true;
        let failed = paint(&mut terminal, "FRAME-TWO").is_err();
        terminal.backend_mut().fail_flush = false;
        assert!(failed);

        // What the tick does when SCREEN_DIRTY is set.
        terminal.clear().unwrap();
        paint(&mut terminal, "FRAME-TWO").unwrap();

        assert_eq!(terminal.backend().row(0), "FRAME-TWO");
    }

    #[test]
    fn bar_fills_proportionally_and_shows_percent() {
        assert!(bar(Some(0.0)).contains("0%"));
        assert!(bar(Some(1.0)).starts_with("[########]"));
        assert!(bar(Some(1.0)).contains("100%"));
        // Overage clamps the fill but still reports the real percentage.
        assert!(bar(Some(1.5)).starts_with("[########]"));
        assert!(bar(Some(1.5)).contains("150%"));
        assert!(bar(None).contains('—'));
    }

    #[test]
    fn fmt_tokens_humanizes() {
        assert_eq!(fmt_tokens(42), "42");
        assert_eq!(fmt_tokens(1_500), "1.5k");
        assert_eq!(fmt_tokens(2_000_000), "2.0M");
    }

    #[test]
    fn fmt_age_buckets() {
        assert_eq!(fmt_age(TimeDuration::seconds(5)), "5s");
        assert_eq!(fmt_age(TimeDuration::seconds(125)), "2m");
        assert_eq!(fmt_age(TimeDuration::hours(3)), "3h");
        assert_eq!(fmt_age(TimeDuration::seconds(-10)), "0s");
    }

    #[test]
    fn truncate_adds_ellipsis_only_when_cut() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("/v1/messages/long/path", 8), "/v1/mes…");
    }

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
            org_uuid: None,
            org_name: None,
            priority: 0,
            status: "active".to_string(),
            disabled: matches!(gate, GateReason::Disabled),
            five_hour: None,
            five_hour_reset: None,
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

    #[test]
    fn rel_buckets() {
        // Under a minute: raw seconds.
        assert_eq!(rel(TimeDuration::seconds(0)), "0s");
        assert_eq!(rel(TimeDuration::seconds(45)), "45s");
        assert_eq!(rel(TimeDuration::seconds(59)), "59s");
        // Under an hour: whole minutes.
        assert_eq!(rel(TimeDuration::seconds(60)), "1m");
        assert_eq!(rel(TimeDuration::minutes(47)), "47m");
        assert_eq!(rel(TimeDuration::minutes(59)), "59m");
        // Under 48h: hours + zero-padded minutes.
        assert_eq!(rel(TimeDuration::hours(1)), "1h00m");
        assert_eq!(rel(TimeDuration::minutes(125)), "2h05m");
        assert_eq!(
            rel(TimeDuration::hours(47) + TimeDuration::minutes(30)),
            "47h30m"
        );
        // 48h and beyond: whole days.
        assert_eq!(rel(TimeDuration::hours(48)), "2d");
        assert_eq!(rel(TimeDuration::days(3)), "3d");
        // Negative (a reset already in the past) clamps to zero.
        assert_eq!(rel(TimeDuration::seconds(-10)), "0s");
    }

    #[test]
    fn gate_chip_labels_each_reason() {
        let now = anchor();
        let at = |secs: i64| Some(now + TimeDuration::seconds(secs));

        assert_eq!(
            gate_chip(&snap_gate("a", GateReason::Ok, None), now).0,
            "OK"
        );
        assert_eq!(
            gate_chip(&snap_gate("a", GateReason::Login, None), now).0,
            "LOGIN"
        );
        assert_eq!(
            gate_chip(&snap_gate("a", GateReason::Disabled, None), now).0,
            "OFF"
        );
        // A hold shows raw seconds ("HOLD 12s").
        assert_eq!(
            gate_chip(&snap_gate("a", GateReason::Hold, at(12)), now).0,
            "HOLD 12s"
        );
        // Quota/Fable gates carry the compact `rel` back-when.
        assert_eq!(
            gate_chip(&snap_gate("a", GateReason::FiveHour, at(47 * 60)), now).0,
            "5H 47m"
        );
        assert_eq!(
            gate_chip(&snap_gate("a", GateReason::SevenDay, at(2 * 86_400)), now).0,
            "7D 2d"
        );
        assert_eq!(
            gate_chip(
                &snap_gate("a", GateReason::FableWeekly, at(3 * 86_400)),
                now
            )
            .0,
            "FABLE-7D 3d"
        );
        // An unknown clear-instant drops the back-when — the display never invents
        // a time the manager could not promise.
        assert_eq!(
            gate_chip(&snap_gate("a", GateReason::FiveHour, None), now).0,
            "5H"
        );
    }

    #[test]
    fn fleet_status_counts_eligible_and_skips_next_free() {
        let now = anchor();
        let accounts = vec![
            snap_gate("a", GateReason::Ok, None),
            snap_gate(
                "b",
                GateReason::FiveHour,
                Some(now + TimeDuration::seconds(300)),
            ),
            snap_gate("c", GateReason::Ok, None),
        ];
        let status = fleet_status(&accounts, now);
        assert_eq!((status.eligible, status.total), (2, 3));
        // Some account is in rotation, so no "next free" is computed.
        assert!(status.next_free.is_none());
    }

    #[test]
    fn fleet_status_names_soonest_recovery_when_none_eligible() {
        let now = anchor();
        // All gated; b returns first (300s) even though it is listed second, and the
        // never-self-freeing Login account is skipped.
        let accounts = vec![
            snap_gate(
                "a",
                GateReason::SevenDay,
                Some(now + TimeDuration::seconds(5_000)),
            ),
            snap_gate(
                "b",
                GateReason::FiveHour,
                Some(now + TimeDuration::seconds(300)),
            ),
            snap_gate("login", GateReason::Login, None),
        ];
        let status = fleet_status(&accounts, now);
        assert_eq!(status.eligible, 0);
        let (name, delta) = status.next_free.expect("some account has a known free_at");
        assert_eq!(name, "b");
        assert_eq!(delta, TimeDuration::seconds(300));
    }

    #[test]
    fn fleet_status_unknown_when_all_gated_without_reset() {
        let now = anchor();
        // Every account is out with NO known clear-instant → next_free is None, so
        // the banner honestly says "unknown" rather than promising a time.
        let accounts = vec![
            snap_gate("a", GateReason::Login, None),
            snap_gate("b", GateReason::Disabled, None),
        ];
        let status = fleet_status(&accounts, now);
        assert_eq!(status.eligible, 0);
        assert!(status.next_free.is_none());
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

    /// The Full layout's minimum width, DERIVED from the columns themselves:
    /// every declared width, one cell of `column_spacing` between each adjacent
    /// pair, and the block's two borders.
    ///
    /// Computed, never transcribed. A test that copies the constant it is meant
    /// to check agrees with a stale value by construction.
    fn derived_full_layout_min_width() -> u16 {
        let columns: u16 = FULL_COLUMN_WIDTHS.iter().sum();
        let gaps = FULL_COLUMN_WIDTHS.len() as u16 - 1;
        let borders = 2;
        columns + gaps + borders
    }

    /// The constant must EQUAL its own columns' derived total, so it cannot drift
    /// from the table it measures.
    ///
    /// This is the assertion that executes the arithmetic the constant's doc
    /// comment merely states. Without it the number is commentary: a fifteenth
    /// column moves the true minimum without moving the constant, and a gate that
    /// only *mentions* the number stays green while the layout clips.
    #[test]
    fn full_layout_min_width_equals_the_derived_total() {
        assert_eq!(
            FULL_LAYOUT_MIN_WIDTH,
            derived_full_layout_min_width(),
            "FULL_LAYOUT_MIN_WIDTH must equal Σ FULL_COLUMN_WIDTHS ({}) + {} inter-column gaps \
             + 2 borders; a column was added or resized without updating it",
            FULL_COLUMN_WIDTHS.iter().sum::<u16>(),
            FULL_COLUMN_WIDTHS.len() - 1,
        );
    }

    #[test]
    fn accounts_layout_picks_mode_at_threshold() {
        // The breakpoint is exact: the derived minimum still fits the full table;
        // one column narrower drops to compact.
        //
        // Fed the DERIVED width, never FULL_LAYOUT_MIN_WIDTH. `accounts_layout` is
        // `width >= FULL_LAYOUT_MIN_WIDTH`, so passing it the constant returns Full
        // for every value that constant could possibly hold — it asserts the
        // comparison operator and learns nothing about the width.
        let derived = derived_full_layout_min_width();
        assert_eq!(accounts_layout(derived), AccountsLayout::Full);
        assert_eq!(accounts_layout(derived - 1), AccountsLayout::Compact);
    }

    #[test]
    fn full_layout_header_and_widths_are_paired() {
        // Two parallel lists indexed against each other. A header added without
        // its width (or the reverse) shifts every column right of the seam.
        assert_eq!(FULL_COLUMNS.len(), FULL_COLUMN_WIDTHS.len());
    }

    /// Read back each Full column's REALISED width from a render at exactly
    /// [`FULL_LAYOUT_MIN_WIDTH`] and hold it to the width that was declared.
    ///
    /// This is the assertion the old one only gestured at. Asserting
    /// `accounts_layout(FULL_LAYOUT_MIN_WIDTH) == Full` compares the constant
    /// against itself and passes for every value it could possibly hold; this one
    /// puts the table through ratatui's constraint solver — the thing that does the
    /// silent squeezing — and fails when the constant is too small for the columns
    /// actually in the table. At the shipped 155 it reports `Gate` realised 11 of
    /// its declared 15.
    #[test]
    fn full_layout_min_width_fits_every_column() {
        let snapshot = util_snapshot(QuotaState::Normal);
        let backend = TestBackend::new(FULL_LAYOUT_MIN_WIDTH, 6);
        let mut terminal = Terminal::new(backend).expect("test backend builds a terminal");
        terminal
            .draw(|frame| render_accounts(frame, frame.area(), &snapshot, 0, anchor()))
            .expect("render succeeds");
        let rows = buffer_rows(terminal.backend().buffer());
        // Row 0 is the block's top border; row 1 is the header. Indexed in CHARS,
        // never bytes: the block's `│` borders are 3 bytes each, so a byte offset
        // reads 2 cells to the right of the cell it names.
        let header: Vec<char> = rows[1].chars().collect();

        // Walk the header left to right, taking each label's offset in turn, so a
        // repeated substring cannot match an earlier column's cell.
        let mut starts = Vec::with_capacity(FULL_COLUMNS.len());
        let mut from = 0usize;
        for label in FULL_COLUMNS {
            let needle: Vec<char> = label.chars().collect();
            let at = (from..=header.len().saturating_sub(needle.len()))
                .find(|i| header[*i..*i + needle.len()] == needle[..])
                .unwrap_or_else(|| {
                    panic!(
                        "header column {label:?} is missing from {:?}",
                        rows[1].as_str()
                    )
                });
            starts.push(at);
            from = at + needle.len();
        }

        // Each column runs to the next column's start, less the 1-cell spacing;
        // the last runs to the block's right border.
        let right_border = usize::from(FULL_LAYOUT_MIN_WIDTH) - 1;
        let realised: Vec<u16> = starts
            .iter()
            .enumerate()
            .map(|(i, start)| {
                let end = starts.get(i + 1).map_or(right_border, |next| next - 1);
                (end - start) as u16
            })
            .collect();

        assert_eq!(
            realised,
            FULL_COLUMN_WIDTHS.to_vec(),
            "at FULL_LAYOUT_MIN_WIDTH ({FULL_LAYOUT_MIN_WIDTH}) every column must realise its \
             declared width; a shortfall means the constant has not followed the columns"
        );
    }

    #[test]
    fn bar_with_reset_shows_countdown_or_dash() {
        let now = anchor();
        // A future reset appends its compact countdown after the bar.
        let future = now + TimeDuration::hours(2) + TimeDuration::minutes(39);
        let with_reset = bar_with_reset(Some(0.53), Some(future), now);
        assert!(
            with_reset.ends_with(" 2h39m"),
            "expected a right-aligned 2h39m countdown, got {with_reset:?}"
        );

        // Never learned: a dash, never an invented number.
        let never_learned = bar_with_reset(Some(0.0), None, now);
        assert!(
            never_learned.ends_with("     -"),
            "no reset learned must render a dash, got {never_learned:?}"
        );

        // Already elapsed: the same dash as never-learned, never a negative
        // countdown.
        let past = now - TimeDuration::minutes(5);
        let elapsed = bar_with_reset(Some(0.9), Some(past), now);
        assert!(
            elapsed.ends_with("     -"),
            "an elapsed reset must render a dash, not a negative countdown, got {elapsed:?}"
        );
    }

    #[test]
    fn pct_renders_percentage_or_dash() {
        // The same number `bar` prints, minus the bar; `—` when never learned.
        assert_eq!(pct(Some(0.47)), " 47%");
        assert_eq!(pct(Some(1.0)), "100%");
        assert_eq!(pct(None), "—");
    }

    #[test]
    fn render_wide_keeps_full_columns_and_bars() {
        // A pane at/above the threshold gets every column and the 8-cell bars.
        let snapshot = util_snapshot(QuotaState::Normal);
        // 10 cells above FULL_LAYOUT_MIN_WIDTH, same margin as before the 5h/7d
        // cells grew a reset countdown (160 -> 174) — a literal `170` here would
        // have silently dropped below the new threshold and rendered Compact.
        let backend = TestBackend::new(FULL_LAYOUT_MIN_WIDTH + 10, 12);
        let mut terminal = Terminal::new(backend).expect("test backend builds a terminal");
        terminal
            .draw(|frame| render_accounts(frame, frame.area(), &snapshot, 0, anchor()))
            .expect("render succeeds");
        let text = buffer_rows(terminal.backend().buffer()).join("\n");

        assert!(text.contains("Probe"), "full mode shows the Probe header");
        assert!(text.contains("Cache"), "full mode shows the Cache header");
        assert!(text.contains('['), "full mode draws at least one bar cell");
    }

    #[test]
    fn render_narrow_shows_percentages_and_drops_probe_cache() {
        // Below the threshold: bar-less percentages, the 7d quota label, and NO
        // Probe/Cache columns — the numbers survive the squeeze by construction.
        let snapshot = util_snapshot(QuotaState::Exhausted);
        let backend = TestBackend::new(104, 12);
        let mut terminal = Terminal::new(backend).expect("test backend builds a terminal");
        terminal
            .draw(|frame| render_accounts(frame, frame.area(), &snapshot, 0, anchor()))
            .expect("render succeeds");
        let text = buffer_rows(terminal.backend().buffer()).join("\n");

        assert!(
            text.contains("Fable"),
            "compact mode keeps the Fable header"
        );
        assert!(text.contains(" 47%"), "compact shows the 5h percentage");
        assert!(text.contains("62%"), "compact shows the Fable percentage");
        assert!(text.contains("full"), "compact keeps the 7d quota label");
        assert!(!text.contains("Probe"), "compact drops the Probe column");
        assert!(!text.contains("Cache"), "compact drops the Cache column");
    }

    /// [`snap_gate`] as a Codex account.
    fn codex_gate(
        name: &str,
        gate: GateReason,
        free_at: Option<OffsetDateTime>,
    ) -> AccountSnapshot {
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
    fn accounts_buffer(
        snapshot: &StatsSnapshot,
        width: u16,
        height: u16,
        selected: usize,
    ) -> Buffer {
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

    /// The x of `needle`'s first character in `row`, counted in cells.
    fn cell_x(row: &str, needle: &str) -> u16 {
        let at = row
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} is not in {row:?}"));
        row[..at].chars().count() as u16
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

    /// The gate chip a Codex row wears speaks ChatGPT's terms. A refusal no full window explains
    /// is REFUSED, with the time left while a hold counts it down; a window that ran out is named
    /// by its length; a Codex member of a reserved group is UNREACHABLE. An upstream 403's short
    /// hold keeps the chip it has on every row. Claude rows keep REJECTED and RESERVED.
    #[test]
    fn codex_chips() {
        let now = anchor();
        let at = |secs: i64| Some(now + TimeDuration::seconds(secs));
        let red = Style::default().fg(Color::Red);
        let red_bold = red.add_modifier(Modifier::BOLD);
        let refused_by_chatgpt = |gate: GateReason, free_at: Option<OffsetDateTime>| {
            let mut row = codex_gate("bob@example.com/codex", gate, free_at);
            row.codex.limit_reached_type = Some("rate_limit_reached".to_string());
            row
        };
        let windowed = |gate: GateReason, minutes: Option<u32>, free_at| {
            let mut row = codex_gate("bob@example.com/codex", gate, free_at);
            match gate {
                GateReason::FiveHour => row.codex.five_hour_window_minutes = minutes,
                _ => row.codex.seven_day_window_minutes = minutes,
            }
            row
        };
        let cases: Vec<(&str, AccountSnapshot, &str, Option<Style>)> = vec![
            (
                "a ChatGPT refusal no window explains",
                codex_gate("bob@example.com/codex", GateReason::Rejected, None),
                "REFUSED",
                Some(red_bold),
            ),
            (
                "ChatGPT's refusal while its hold counts down",
                refused_by_chatgpt(GateReason::Hold, at(47 * 60 + 30)),
                "REFUSED 47m",
                Some(red),
            ),
            (
                "ChatGPT's refusal with no known end",
                refused_by_chatgpt(GateReason::Hold, None),
                "REFUSED",
                Some(red_bold),
            ),
            (
                "a rate-limit hold that is not a refusal",
                codex_gate("bob@example.com/codex", GateReason::Hold, at(12)),
                "HOLD 12s",
                None,
            ),
            (
                "ChatGPT's refusal under an upstream 403's hold",
                refused_by_chatgpt(GateReason::Refused, at(107)),
                "REFUSED 1m",
                Some(red),
            ),
            (
                "an upstream 403's hold alone",
                codex_gate("bob@example.com/codex", GateReason::Refused, at(107)),
                "REFUSED 107s",
                Some(red),
            ),
            (
                "a Codex member of a reserved group",
                codex_gate("bob@example.com/codex", GateReason::Reserved, None),
                "UNREACHABLE",
                Some(red_bold),
            ),
            (
                "a 30-day window",
                windowed(GateReason::SevenDay, Some(43_200), at(12 * 86_400 + 30)),
                "30D 12d",
                Some(red),
            ),
            (
                "a weekly window of no reported length",
                windowed(GateReason::SevenDay, None, at(3 * 86_400 + 30)),
                "7D 3d",
                Some(red),
            ),
            (
                "a five-hour window",
                windowed(
                    GateReason::FiveHour,
                    Some(300),
                    at(2 * 3_600 + 59 * 60 + 30),
                ),
                "5H 2h59m",
                Some(red),
            ),
            (
                "Anthropic's own verdict",
                snap_gate("alice@example.com", GateReason::Rejected, None),
                "REJECTED",
                None,
            ),
            (
                "a Claude member of a reserved group",
                snap_gate("alice@example.com", GateReason::Reserved, None),
                "RESERVED",
                None,
            ),
            (
                "an upstream 403's hold on a Claude row",
                snap_gate("alice@example.com", GateReason::Refused, at(107)),
                "REFUSED 107s",
                Some(red),
            ),
            (
                "an upstream 403's hold on a Claude row, no end known",
                snap_gate("alice@example.com", GateReason::Refused, None),
                "REFUSED",
                None,
            ),
        ];
        for (what, row, text, style) in cases {
            let (label, drawn) = gate_chip(&row, now);
            assert_eq!(label, text, "{what}");
            if let Some(style) = style {
                assert_eq!(drawn, style, "{what}");
            }
        }
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

    /// Each section marks the account serving its own provider. One marker for the whole pane
    /// showed only whichever provider served last, so the account serving the other one
    /// vanished from the screen every time it did.
    #[test]
    fn one_cursor_marker_per_provider() {
        let mut snapshot = interleaved_fleet();
        snapshot.current_by_provider = crate::stats::CurrentByProvider {
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
            rows[0].starts_with("┌ teamclaude-rs · accounts · claude (2) "),
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
            rows[0].starts_with("┌ teamclaude-rs · accounts · codex (2) "),
            "{}",
            rows.join("\n")
        );

        let claude_only = snapshot_of(vec![
            snap_gate("alice@example.com", GateReason::Ok, None),
            snap_gate("frank@example.com", GateReason::Ok, None),
        ]);
        let rows = buffer_rows(&accounts_buffer(&claude_only, FULL_LAYOUT_MIN_WIDTH, 12, 0));
        assert!(
            rows[0].starts_with("┌ teamclaude-rs · accounts (2) "),
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
            rows[0].starts_with("┌ teamclaude-rs · accounts · claude (2/3 ▼) "),
            "{pane}"
        );
        assert!(
            rows.iter().any(|row| row.starts_with("├ codex (3/5 ▼) ")),
            "{pane}"
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
        let config: crate::config::Config =
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
            reanchor(&drawn, 1, &[frank.clone(), bob.clone()]),
            0,
            "alice was deleted above frank, and the cursor stays on frank"
        );
        assert_eq!(
            reanchor(
                &[alice.clone(), frank.clone()],
                1,
                &[alice.clone(), frank.clone(), carol.clone()]
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

        let notice = toggle_notice(DisablePersist::NoSuchAccount, false, None)
            .expect("no row is never saved");
        assert_eq!(
            Some(notice.text),
            DisablePersist::NoSuchAccount.warning(false)
        );
        assert_eq!(notice.account, None);

        assert!(toggle_notice(DisablePersist::Persisted, true, Some(&bob)).is_none());
    }

    /// What the dashboard asks the process each tick is gated on its fleet, as every Codex
    /// surface is: a Claude-only fleet asks nothing about codex at all, an empty one asks only
    /// whether a codex login is on disk, and a fleet holding Codex rows asks for the route with
    /// their count and never for a login.
    #[test]
    fn dashboard_facts_are_gated_on_the_fleet() {
        let home = Some(PathBuf::from("/Users/example"));
        let claude_only = snapshot_of(vec![snap_gate("alice@example.com", GateReason::Ok, None)]);
        let facts = dashboard_facts(
            &claude_only,
            home.clone(),
            |_| panic!("a Claude-only fleet asks for no Codex route"),
            || panic!("a Claude-only fleet asks for no Codex traffic"),
            || panic!("a fleet with accounts checks for no codex login"),
        );
        assert_eq!(facts.codex_route, None);
        assert_eq!(facts.ca_path, None);
        assert_eq!(facts.codex_traffic, None);
        assert_eq!(facts.codex_login, None);
        assert_eq!(facts.home, home);

        let login = PathBuf::from("/Users/example/.codex/auth.json");
        let facts = dashboard_facts(
            &snapshot_of(vec![]),
            home.clone(),
            |_| panic!("an empty fleet asks for no Codex route"),
            || panic!("an empty fleet asks for no Codex traffic"),
            || Some(login.clone()),
        );
        assert_eq!(facts.codex_login, Some(login));

        let asked = std::cell::Cell::new(None);
        let ca = PathBuf::from("/Users/example/.config/tcr-ca.pem");
        let facts = dashboard_facts(
            &snapshot_of(vec![
                snap_gate("alice@example.com", GateReason::Ok, None),
                codex_gate("bob@example.com/codex", GateReason::Ok, None),
            ]),
            home,
            |codex_rows| {
                asked.set(Some(codex_rows));
                (
                    Some(CodexRoute {
                        pooled: true,
                        ..Default::default()
                    }),
                    Some(ca.clone()),
                )
            },
            || Some(CodexTraffic::default()),
            || panic!("a fleet with accounts checks for no codex login"),
        );
        assert_eq!(asked.get(), Some(1), "asked with its one Codex row");
        assert!(facts.codex_route.is_some_and(|route| route.pooled));
        assert_eq!(facts.ca_path, Some(ca));
        assert_eq!(facts.codex_traffic, Some(CodexTraffic::default()));
        assert_eq!(facts.codex_login, None);
    }

    /// A manager over the inline `accounts` array, its config written to a fresh file in the
    /// temp folder, never a real one.
    fn manager_on_disk(tag: &str, accounts: &str) -> (Arc<Manager>, PathBuf) {
        let config: crate::config::Config =
            serde_json::from_str(&format!(r#"{{"accounts": {accounts}}}"#))
                .expect("the inline test config parses");
        let path = std::env::temp_dir().join(format!(
            "tcr-tui-{tag}-{}-{}.json",
            std::process::id(),
            crate::now_ms()
        ));
        crate::config::save(&path, &config).expect("write the test config");
        (
            Manager::with_live_refresher(config, Some(path.clone())),
            path,
        )
    }

    /// Rewrite the config at `path` to hold only `accounts`, as `tcr remove` does while the
    /// dashboard runs. The sleep moves the file's mtime, which is what the reload watches.
    fn rewrite_config(path: &std::path::Path, accounts: &str) {
        std::thread::sleep(Duration::from_millis(20));
        let config: crate::config::Config =
            serde_json::from_str(&format!(r#"{{"accounts": {accounts}}}"#))
                .expect("the inline test config parses");
        crate::config::save(path, &config).expect("rewrite the test config");
    }

    /// A row deleted from the config keeps its slot and leaves the list, so the slot, never the
    /// position, is what a key acts on, and a frame drawn before the delete cannot reach it.
    #[test]
    fn a_key_never_reaches_a_row_deleted_from_the_config() {
        let (manager, path) =
            manager_on_disk("deleted-row-key", &format!("[{ALICE}, {BOB}, {FRANK}]"));
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
            crate::manager::DisablePersist::NoSuchAccount.warning(false),
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

    /// The text of `line`, its spans joined.
    fn line_text(line: &Line) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    /// The style of the one span of `line` whose text is `text`, or a panic naming the spans.
    fn span_style(line: &Line, text: &str) -> Style {
        let Some(span) = line.spans.iter().find(|span| span.content == text) else {
            panic!("no span reads {text:?}: {:?}", line.spans);
        };
        span.style
    }

    fn green_bold() -> Style {
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD)
    }

    fn dim() -> Style {
        Style::default().add_modifier(Modifier::DIM)
    }

    /// The banner gives each provider a segment of its own, counted over its own accounts and
    /// naming its own next free account, so it says what each client's 429 says: a spent Claude
    /// fleet is never shown green because a Codex account is ready. Each segment takes its own
    /// style; a fleet of one provider keeps one segment.
    #[test]
    fn fleet_banner_splits_by_provider() {
        let now = anchor();
        let at = |secs: i64| Some(now + TimeDuration::seconds(secs));
        let banner = |accounts: Vec<AccountSnapshot>| {
            fleet_banner_line(&snapshot_of(accounts), &DashboardFacts::default(), now, 174)
        };

        let claude_out = banner(vec![
            snap_gate(
                "alice@example.com",
                GateReason::FiveHour,
                at(2 * 3_600 + 59 * 60 + 30),
            ),
            snap_gate(
                "frank@example.com",
                GateReason::SevenDay,
                at(3 * 86_400 + 30),
            ),
            snap_gate("erin@example.com", GateReason::Login, None),
            codex_gate("bob@example.com/codex", GateReason::Ok, None),
            codex_gate("dana@example.com", GateReason::Ok, None),
        ]);
        assert_eq!(
            line_text(&claude_out),
            "CLAUDE 0/3 eligible · next free alice@example.com in 2h59m │ CODEX 2/2 eligible"
        );
        assert_eq!(
            span_style(
                &claude_out,
                "CLAUDE 0/3 eligible · next free alice@example.com in 2h59m"
            ),
            alarm_style()
        );
        assert_eq!(span_style(&claude_out, " │ "), dim());
        assert_eq!(span_style(&claude_out, "CODEX 2/2 eligible"), green_bold());

        let codex_out = banner(vec![
            snap_gate("alice@example.com", GateReason::Ok, None),
            snap_gate("frank@example.com", GateReason::Ok, None),
            codex_gate(
                "bob@example.com/codex",
                GateReason::SevenDay,
                at(4 * 86_400 + 30),
            ),
            codex_gate(
                "carol@example.com",
                GateReason::SevenDay,
                at(3 * 86_400 + 30),
            ),
        ]);
        assert_eq!(
            line_text(&codex_out),
            "CLAUDE 2/2 eligible │ CODEX 0/2 eligible · next free carol@example.com in 3d"
        );
        assert_eq!(span_style(&codex_out, "CLAUDE 2/2 eligible"), green_bold());
        assert_eq!(
            span_style(
                &codex_out,
                "CODEX 0/2 eligible · next free carol@example.com in 3d"
            ),
            alarm_style()
        );

        let codex_only = banner(vec![
            codex_gate("bob@example.com/codex", GateReason::Ok, None),
            codex_gate("dana@example.com", GateReason::Ok, None),
            codex_gate(
                "carol@example.com",
                GateReason::SevenDay,
                at(3 * 86_400 + 30),
            ),
            codex_gate("erin@example.com", GateReason::Login, None),
        ]);
        assert_eq!(line_text(&codex_only), "CODEX 2/4 eligible");
        assert_eq!(span_style(&codex_only, "CODEX 2/4 eligible"), green_bold());

        let both_out = banner(vec![
            snap_gate(
                "alice@example.com",
                GateReason::FiveHour,
                at(2 * 3_600 + 59 * 60 + 30),
            ),
            codex_gate(
                "bob@example.com/codex",
                GateReason::FiveHour,
                at(5 * 60 + 30),
            ),
        ]);
        assert_eq!(
            line_text(&both_out),
            "CLAUDE 0/1 eligible · next free alice@example.com in 2h59m │ \
             CODEX 0/1 eligible · next free bob@example.com/codex in 5m",
            "each tail names its own provider's account"
        );

        let claude_only = banner(vec![
            snap_gate("alice@example.com", GateReason::FiveHour, at(47 * 60 + 30)),
            snap_gate("frank@example.com", GateReason::Login, None),
        ]);
        assert_eq!(
            claude_only.spans,
            vec![Span::styled(
                "FLEET 0/2 eligible · next free alice@example.com in 47m",
                alarm_style()
            )],
            "a Claude-only fleet keeps its one FLEET segment"
        );
    }

    /// A fleet of two Claude accounts out and a Codex pool whose two accounts are both spent,
    /// carol first: the fleet the fitting tests shorten.
    fn spent_codex_pool() -> StatsSnapshot {
        let now = anchor();
        let at = |secs: i64| Some(now + TimeDuration::seconds(secs));
        snapshot_of(vec![
            snap_gate("alice@example.com", GateReason::Ok, None),
            snap_gate("frank@example.com", GateReason::FiveHour, at(3_600 + 30)),
            snap_gate("erin@example.com", GateReason::Login, None),
            codex_gate(
                "bob@example.com/codex",
                GateReason::SevenDay,
                at(4 * 86_400 + 30),
            ),
            codex_gate(
                "carol@example.com",
                GateReason::SevenDay,
                at(3 * 86_400 + 30),
            ),
        ])
    }

    /// A route that does not pool codex, with `code`.
    fn not_pooled(code: &str) -> CodexRoute {
        CodexRoute {
            pooled: false,
            code: Some(code.to_string()),
            reason: Some(format!("the launcher's sentence for {code}")),
            upstream: None,
        }
    }

    /// When codex is not pooled the CODEX segment says why, in a few words keyed on the route's
    /// code: the same decision `tcr status` words in full.
    #[test]
    fn not_pooled_suffix_uses_codex_route() {
        let snapshot = snapshot_of(vec![codex_gate(
            "bob@example.com/codex",
            GateReason::Ok,
            None,
        )]);
        let yellow = Style::default().fg(Color::Yellow);
        for (code, suffix) in [
            (
                "js-leaf",
                " · not pooled: JS leaf covers api.anthropic.com only",
            ),
            (
                "mitm-off",
                " · not pooled: MITM off (TLS material failed to load)",
            ),
            (
                "leaf-missing-host",
                " · not pooled: leaf does not cover chatgpt.com",
            ),
            (
                "no-ca",
                " · not pooled: no readable CA at /Users/example/.config/tcr-ca.pem",
            ),
            ("unknown-leaf", " · not pooled: leaf state unknown"),
        ] {
            let facts = DashboardFacts {
                codex_route: Some(not_pooled(code)),
                ca_path: Some(PathBuf::from("/Users/example/.config/tcr-ca.pem")),
                ..DashboardFacts::default()
            };
            let line = fleet_banner_line(&snapshot, &facts, anchor(), 174);
            assert_eq!(
                line_text(&line),
                format!("CODEX 1/1 eligible{suffix}"),
                "{code}"
            );
            assert_eq!(span_style(&line, suffix), yellow, "{code}");
        }

        let pooled = DashboardFacts {
            codex_route: Some(CodexRoute {
                pooled: true,
                ..Default::default()
            }),
            ..DashboardFacts::default()
        };
        assert_eq!(
            line_text(&fleet_banner_line(&snapshot, &pooled, anchor(), 174)),
            "CODEX 1/1 eligible"
        );
    }

    /// A banner that does not fit gives up its least urgent words first: the unpooled count,
    /// then the refused count, then the names in the next-free tails, then the tails. The
    /// counts themselves always stay.
    #[test]
    fn the_banner_drops_unpooled_then_refused_then_shortens_next_free() {
        let snapshot = spent_codex_pool();
        let facts = DashboardFacts {
            codex_traffic: Some(CodexTraffic {
                refused_10m: 12,
                unpooled_10m: 3,
                ..Default::default()
            }),
            ..DashboardFacts::default()
        };
        let line = |width: u16| fleet_banner_line(&snapshot, &facts, anchor(), width);

        let full = line(116);
        assert_eq!(
            line_text(&full),
            "CLAUDE 1/3 eligible │ CODEX 0/2 eligible · next free carol@example.com in 3d \
             · 12 refused in 10m · 3 unpooled in 10m"
        );
        assert_eq!(
            span_style(&full, " · 12 refused in 10m"),
            Style::default().fg(Color::Yellow)
        );
        assert_eq!(span_style(&full, " · 3 unpooled in 10m"), dim());
        assert_eq!(
            line_text(&line(110)),
            "CLAUDE 1/3 eligible │ CODEX 0/2 eligible · next free carol@example.com in 3d \
             · 12 refused in 10m"
        );
        assert_eq!(
            line_text(&line(90)),
            "CLAUDE 1/3 eligible │ CODEX 0/2 eligible · next free carol@example.com in 3d"
        );
        assert_eq!(
            line_text(&line(70)),
            "CLAUDE 1/3 eligible │ CODEX 0/2 eligible · next free in 3d"
        );
    }

    /// A not-pooled reason is never dropped: when nothing else is left to give, the reason is
    /// cut, visibly, and ` · not pooled: ` stays whole.
    #[test]
    fn not_pooled_reason_is_truncated_visibly() {
        let facts = DashboardFacts {
            codex_route: Some(not_pooled("no-ca")),
            ca_path: Some(PathBuf::from("/Users/example/.config/tcr-ca.pem")),
            ..DashboardFacts::default()
        };
        let line = fleet_banner_line(&spent_codex_pool(), &facts, anchor(), 100);
        let text = line_text(&line);
        assert_eq!(
            text,
            "CLAUDE 1/3 eligible │ CODEX 0/2 eligible · not pooled: no readable CA at \
             /Users/example/.config/tcr…"
        );
        assert_eq!(text.chars().count(), 100);
        assert_eq!(
            span_style(
                &line,
                " · not pooled: no readable CA at /Users/example/.config/tcr…"
            ),
            Style::default().fg(Color::Yellow)
        );
    }

    /// An empty fleet on a Mac where the codex CLI has a login of its own says how to add a
    /// Codex account, and where that login is, by its existence alone. Its banner is not the
    /// alarm, since nothing is down: there is nothing yet. Without such a login, the empty
    /// fleet draws as it always did.
    #[test]
    fn empty_fleet_with_codex_login_drops_next_free() {
        let empty = snapshot_of(vec![]);
        let facts = DashboardFacts {
            codex_login: Some(PathBuf::from("/Users/example/.codex/auth.json")),
            home: Some(PathBuf::from("/Users/example")),
            ..DashboardFacts::default()
        };
        let banner = fleet_banner_line(&empty, &facts, anchor(), 110);
        assert_eq!(
            banner.spans,
            vec![Span::styled("FLEET 0/0 eligible", dim())]
        );

        let draw = |facts: &DashboardFacts| {
            let mut terminal =
                Terminal::new(TestBackend::new(110, 12)).expect("test backend builds a terminal");
            terminal
                .draw(|frame| render_with(frame, &empty, 0, None, facts, anchor()))
                .expect("render succeeds");
            terminal.backend().buffer().clone()
        };
        let buffer = draw(&facts);
        let rows = buffer_rows(&buffer);
        let (y, row) = row_holding(
            &rows,
            "no accounts — tcr login (Claude) · tcr login --codex (Codex)",
        );
        let x = cell_x(row, "no accounts");
        assert_eq!(buffer[(x, y as u16)].modifier, Modifier::empty(), "{row}");
        let (y, row) = row_holding(
            &rows,
            "found a codex login at ~/.codex/auth.json — tcr login --codex gives tcr a login of its own",
        );
        let x = cell_x(row, "found");
        assert!(
            buffer[(x, y as u16)].modifier.contains(Modifier::DIM),
            "{row}"
        );

        let without = draw(&DashboardFacts::default());
        let mut terminal =
            Terminal::new(TestBackend::new(110, 12)).expect("test backend builds a terminal");
        terminal
            .draw(|frame| render(frame, &empty, 0, None))
            .expect("render succeeds");
        assert_eq!(
            &without,
            terminal.backend().buffer(),
            "with no codex login, the empty fleet draws as it always did"
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
            let mut terminal = Terminal::new(TestBackend::new(width, height))
                .expect("test backend builds a terminal");
            terminal
                .draw(|frame| {
                    render_accounts_with(frame, frame.area(), snapshot, selected, &facts, anchor())
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
            crate::stats::RequestLogEntry {
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

    /// What `tcr demo` draws on its first tick, cursor on the first row and no notice, at the
    /// Full layout's width and at a Compact one: both providers, each in its own banner segment
    /// and its own section with its own ▶, and Codex traffic in the sessions tree and the log.
    /// `--nocapture` prints both frames.
    #[test]
    fn the_demo_draws_both_providers() {
        let manager = crate::demo::demo_manager();
        let facts = crate::demo::demo_facts();
        let now = OffsetDateTime::now_utc();
        let snapshot = manager.snapshot(now);
        for width in [FULL_LAYOUT_MIN_WIDTH, 110] {
            let mut terminal =
                Terminal::new(TestBackend::new(width, 40)).expect("test backend builds a terminal");
            terminal
                .draw(|frame| render_with(frame, &snapshot, 0, None, &facts, now))
                .expect("render succeeds");
            let rows = buffer_rows(terminal.backend().buffer());
            let frame = rows.join("\n");
            println!("== {width}x40\n{frame}");

            assert!(
                rows[0].starts_with("CLAUDE ") && rows[0].contains(" │ CODEX "),
                "{frame}"
            );
            assert!(
                rows.iter()
                    .any(|row| row.starts_with("┌ teamclaude-rs · accounts · claude (6) ")),
                "{frame}"
            );
            assert!(
                rows.iter().any(|row| row.starts_with("├ codex (3) ")),
                "{frame}"
            );
            let marked: Vec<&String> = rows.iter().filter(|row| row.starts_with("│▶ ")).collect();
            assert_eq!(marked.len(), 2, "one ▶ per provider\n{frame}");
            assert!(
                marked.iter().any(|row| row.contains("team-prod")),
                "{frame}"
            );
            assert!(
                marked.iter().any(|row| row.contains("bob@e…/codex")),
                "{frame}"
            );
            let (_, bob) = row_holding(&rows, "bob@e…/codex");
            assert!(bob.contains("none"), "{frame}");
            let (_, carol) = row_holding(&rows, "carol@examp…");
            assert!(carol.contains("7D 3d"), "{frame}");
            row_holding(&rows, "▾ bob@exa…/codex · 1 codex");
            row_holding(&rows, "…/codex/responses");
        }
    }

    /// The person who pressed `d` must find out the write failed WITHOUT reading a
    /// log file. `tracing` goes to a file in TUI mode, so a failed persist was
    /// invisible while the row kept rendering as benched — the warning has to be on
    /// screen, and it has to cost a row rather than paint over the fleet banner.
    #[test]
    fn render_shows_a_failed_persist_notice_without_hiding_the_fleet_banner() {
        let snapshot = util_snapshot(QuotaState::Normal);
        let warning = crate::manager::DisablePersist::NoEntry
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

    /// The alarm styling is defined once. Two literals would drift, and a notice
    /// that stopped looking like the fleet-down banner would read as ordinary
    /// chrome — which is exactly what it is not.
    #[test]
    fn the_notice_and_the_downed_fleet_banner_share_one_alarm_style() {
        let style = alarm_style();
        assert_eq!(style.fg, Some(Color::Red));
        assert!(style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn render_sessions_keeps_a_diverted_session_under_its_pin() {
        // One session pinned to `alice` whose most recent request was diverted to
        // `bob`. The pane must show it as alice's — with the divert marked — and
        // must NOT open a bob group, because no pin moved.
        let snapshot = StatsSnapshot {
            accounts: vec![],
            current: None,
            current_by_provider: crate::stats::CurrentByProvider::default(),
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

    /// A session sitting on its pin, last seen at `last_seen`. The golden times its rows from
    /// the clock `render` reads, so [`snap`]'s seconds after the epoch do not fit it.
    fn snap_at(
        id: &str,
        account: &str,
        requests: u64,
        last_seen: OffsetDateTime,
    ) -> SessionSnapshot {
        SessionSnapshot {
            last_seen: Some(last_seen),
            ..snap(id, account, requests, None)
        }
    }

    /// [`diverted`], last seen at `last_seen`.
    fn diverted_at(
        id: &str,
        account: &str,
        served_by: &str,
        requests: u64,
        last_seen: OffsetDateTime,
    ) -> SessionSnapshot {
        SessionSnapshot {
            last_seen: Some(last_seen),
            ..diverted(id, account, served_by, requests, None)
        }
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

    /// The Claude-only fleet the golden pins: eight accounts, one in each gate the dashboard
    /// draws a chip for, three sessions (one of them diverted) and three log lines.
    ///
    /// HOLD is left out on purpose: its chip counts raw seconds, so two runs a second apart
    /// would draw two different frames. Every other time sits a whole unit plus 30 seconds from
    /// `now`, so the few milliseconds between building this snapshot and `render` reading its own
    /// clock cannot move a printed age or countdown.
    fn claude_only_golden_snapshot(now: OffsetDateTime, alice_eligible: bool) -> StatsSnapshot {
        let after = |secs: i64| now + TimeDuration::seconds(secs);
        let before = |secs: i64| now - TimeDuration::seconds(secs);
        const MINUTE: i64 = 60;
        const HOUR: i64 = 3_600;
        const DAY: i64 = 86_400;

        let mut alice = if alice_eligible {
            snap_gate("alice@example.com", GateReason::Ok, None)
        } else {
            snap_gate(
                "alice@example.com",
                GateReason::FiveHour,
                Some(after(47 * MINUTE + 30)),
            )
        };
        alice.organization_type = Some("claude_max".to_string());
        alice.rate_limit_tier = Some("default_claude_max_20x".to_string());
        alice.five_hour = Some(0.47);
        alice.five_hour_reset = Some(after(2 * HOUR + 39 * MINUTE + 30));
        alice.seven_day = Some(0.20);
        alice.seven_day_reset = Some(after(4 * DAY + 30));
        alice.seven_day_oi = Some(0.62);
        alice.seven_day_oi_reset = Some(after(4 * DAY + 30));
        alice.requests = 1200;
        alice.input_tokens = 4_200_000;
        alice.output_tokens = 310_000;
        alice.cache_read_tokens = 3_900_000;
        alice.last_used = Some(before(3 * MINUTE + 30));
        alice.probe_status = ProbeStatus::Ok;
        alice.last_probe = Some(before(12 * MINUTE + 30));

        let mut frank = snap_gate(
            "frank@example.com",
            GateReason::SevenDay,
            Some(after(3 * DAY + 30)),
        );
        frank.organization_type = Some("claude_team".to_string());
        frank.rate_limit_tier = Some("default_claude_max_5x".to_string());
        frank.seat_tier = Some("team_tier_1".to_string());
        frank.five_hour = Some(0.10);
        frank.five_hour_reset = Some(after(4 * HOUR + 10 * MINUTE + 30));
        frank.seven_day = Some(0.96);
        frank.seven_day_reset = Some(after(3 * DAY + 30));
        frank.quota_state = QuotaState::NearLimit;
        frank.probe_status = ProbeStatus::RateLimited;
        frank.last_probe = Some(before(2 * MINUTE + 30));
        frank.stream_error_count = 2;
        frank.requests = 2100;

        let mut erin = snap_gate("erin@example.com", GateReason::Login, None);
        erin.status = "error".to_string();
        erin.probe_status = ProbeStatus::Error;
        erin.last_probe = Some(before(45 * MINUTE + 30));

        let mut grace = snap_gate("grace@example.com", GateReason::Disabled, None);
        grace.organization_type = Some("claude_pro".to_string());
        grace.seven_day = Some(0.30);
        grace.seven_day_reset = Some(after(5 * DAY + 30));

        let mut henry = snap_gate("henry@example.com", GateReason::Reserved, None);
        henry.seven_day = Some(1.00);
        henry.seven_day_reset = Some(after(2 * DAY + 30));
        henry.quota_state = QuotaState::Exhausted;
        henry.groups = vec!["research".to_string()];
        henry.reserved_groups = vec!["research".to_string()];

        let mut ivy = snap_gate("ivy@example.com", GateReason::Rejected, None);
        ivy.five_hour = Some(0.88);
        ivy.five_hour_reset = Some(after(HOUR + 30));

        let mut jack = snap_gate(
            "jack@example.com",
            GateReason::FableWeekly,
            Some(after(2 * DAY + 30)),
        );
        jack.seven_day_oi = Some(1.00);
        jack.seven_day_oi_reset = Some(after(2 * DAY + 30));

        let mut kate = snap_gate("kate@example.com", GateReason::Parked, None);
        kate.groups = vec!["orchard".to_string()];
        kate.parked_groups = vec!["orchard".to_string()];

        StatsSnapshot {
            accounts: vec![alice, frank, erin, grace, henry, ivy, jack, kate],
            current: Some(0),
            current_by_provider: crate::stats::CurrentByProvider::default(),
            recent: vec![
                log_entry(
                    200,
                    "POST",
                    "/v1/messages",
                    "alice@example.com",
                    before(MINUTE + 30),
                ),
                log_entry(
                    429,
                    "POST",
                    "/v1/messages",
                    "frank@example.com",
                    before(2 * MINUTE + 30),
                ),
                log_entry(
                    500,
                    "GET",
                    "/v1/messages/count_tokens?beta=true",
                    "ivy@example.com",
                    before(3 * MINUTE + 30),
                ),
            ],
            sessions: vec![
                snap_at("a1f3c2", "alice@example.com", 24, before(MINUTE + 30)),
                diverted_at(
                    "b2c7d4",
                    "alice@example.com",
                    "frank@example.com",
                    6,
                    before(2 * MINUTE + 30),
                ),
                snap_at("c3d8e5", "frank@example.com", 12, before(5 * MINUTE + 30)),
            ],
            wire_sessions: vec![],
            wire_sessions_summary: tcr_status_wire::SessionsSummary::default(),
        }
    }

    /// A snapshot with no accounts, no sessions and no log: a fleet nobody has logged into.
    fn empty_snapshot() -> StatsSnapshot {
        StatsSnapshot {
            accounts: vec![],
            current: None,
            current_by_provider: crate::stats::CurrentByProvider::default(),
            recent: vec![],
            sessions: vec![],
            wire_sessions: vec![],
            wire_sessions_summary: tcr_status_wire::SessionsSummary::default(),
        }
    }

    /// The switch that rewrites the golden below instead of comparing against it.
    const UPDATE_TUI_GOLDEN: &str = "TCR_UPDATE_TUI_GOLDEN";

    /// **The Claude-only guarantee for the dashboard.** Four frames of a fleet with no Codex
    /// account, captured from the dashboard before it learned about Codex accounts: 174 columns
    /// (the full layout) and 110 columns (the compact one) with one account eligible, 110
    /// columns with nothing eligible, and an empty fleet. The frames are ratatui's `Buffer`
    /// debug form, which prints every cell's text and every change of style, so a colour or a
    /// modifier that moves fails this as surely as a character does.
    ///
    /// The golden is never regenerated to fit a change. A Claude-only frame that differs from it
    /// is a change a Claude-only user sees, and that is the bug.
    #[test]
    fn claude_only_frames_are_unchanged() {
        let mut text = String::new();
        let frames: [(&str, u16, Option<bool>); 4] = [
            (
                "174 columns, one account eligible",
                FULL_LAYOUT_MIN_WIDTH,
                Some(true),
            ),
            ("110 columns, one account eligible", 110, Some(true)),
            ("110 columns, nothing eligible", 110, Some(false)),
            ("110 columns, an empty fleet", 110, None),
        ];
        for (label, width, alice_eligible) in frames {
            let snapshot = match alice_eligible {
                Some(eligible) => claude_only_golden_snapshot(OffsetDateTime::now_utc(), eligible),
                None => empty_snapshot(),
            };
            let mut terminal =
                Terminal::new(TestBackend::new(width, 40)).expect("test backend builds a terminal");
            terminal
                .draw(|frame| render(frame, &snapshot, 2, None))
                .expect("render succeeds");
            text.push_str(&format!("== {label}\n{:?}\n", terminal.backend().buffer()));
        }

        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/tui-claude-only.golden");
        if std::env::var_os(UPDATE_TUI_GOLDEN).is_some() {
            std::fs::write(&path, &text)
                .unwrap_or_else(|err| panic!("cannot write {}: {err}", path.display()));
            return;
        }
        let golden = std::fs::read_to_string(&path).unwrap_or_else(|err| {
            panic!(
                "cannot read the golden at {} ({err}); it is written once, by \
                 {UPDATE_TUI_GOLDEN}=1, before the change it guards",
                path.display()
            )
        });
        assert_eq!(
            text, golden,
            "a Claude-only dashboard frame changed. The golden is never regenerated with \
             {UPDATE_TUI_GOLDEN} to fit a change: a Claude-only frame that differs from it is a \
             change every Claude-only user sees"
        );
    }

    /// Is `cell` painted in exactly `style`: its colour where `style` sets one, and exactly its
    /// modifiers?
    fn painted_with(cell: &ratatui::buffer::Cell, style: Style) -> bool {
        style.fg.is_none_or(|fg| cell.fg == fg)
            && style.bg.is_none_or(|bg| cell.bg == bg)
            && cell.modifier == style.add_modifier
    }

    /// An empty fleet with no codex login on disk is today's, word for word: the alarm banner
    /// with no time to promise, and the empty accounts pane. Nothing on it may mention codex.
    #[test]
    fn empty_fleet_without_evidence_is_unchanged() {
        let snapshot = empty_snapshot();
        let mut terminal =
            Terminal::new(TestBackend::new(110, 12)).expect("test backend builds a terminal");
        terminal
            .draw(|frame| render(frame, &snapshot, 0, None))
            .expect("render succeeds");
        let buffer = terminal.backend().buffer();
        let rows = buffer_rows(buffer);
        let screen = rows.join("\n");

        assert_eq!(
            rows[0].trim_end(),
            "FLEET 0/0 eligible · next free unknown",
            "{screen}"
        );
        assert!(
            painted_with(&buffer[(0, 0)], alarm_style()),
            "the banner of an empty fleet is the alarm: {:?}",
            buffer[(0, 0)]
        );
        assert!(
            rows.iter()
                .any(|row| row.contains(" teamclaude-rs · accounts (0) ")),
            "{screen}"
        );
        assert!(
            !rows.iter().any(|row| row.to_lowercase().contains("codex")),
            "an empty fleet with no codex login says nothing about codex:\n{screen}"
        );
    }
}
