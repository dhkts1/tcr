//! Terminal safety: the guard that hands the terminal back on every exit path, the
//! panic hook, and the repaint that repairs a screen a failed write left out of step.

use std::io::{self};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::log_err::LogErr;
use ratatui::crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::{Frame, Terminal};

/// Restores the terminal to a sane state whenever it is dropped — normal exit,
/// an early `?`, or a panic unwind. Constructing it enters raw-mode + the
/// alternate screen; dropping it leaves them, best-effort (a `Drop` never panics).
pub(super) struct TerminalGuard;

impl TerminalGuard {
    pub(super) fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        disable_raw_mode().log_warn("leaving raw mode");
        execute!(io::stdout(), LeaveAlternateScreen, DisableBracketedPaste)
            .log_warn("leaving the alternate screen");
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
pub(super) static SCREEN_DIRTY: AtomicBool = AtomicBool::new(false);

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
pub(super) fn repaint<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    dirty: &AtomicBool,
    render_frame: impl FnOnce(&mut Frame),
) {
    if dirty.swap(false, Ordering::SeqCst)
        && let Err(err) = repair_screen(terminal)
    {
        // The repair did not land either. Stay dirty and retry on the next tick
        // rather than dropping it on the floor.
        tracing::warn!(error = %err, "could not clear a dirty screen");
        dirty.store(true, Ordering::SeqCst);
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
pub(super) fn install_panic_hook() {
    use std::sync::Once;
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let thread = std::thread::current();
            if panic_takes_the_process_down(thread.name()) {
                disable_raw_mode().log_warn("leaving raw mode after a panic");
                execute!(io::stdout(), LeaveAlternateScreen, DisableBracketedPaste)
                    .log_warn("leaving the alternate screen after a panic");
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
