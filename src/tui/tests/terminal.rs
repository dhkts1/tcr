//! Tests of the repaint over a backend whose flush can fail, and the screen repair.

use super::*;

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

fn paint(terminal: &mut Terminal<PendingBackend>, text: &'static str) -> Result<(), FlushFailure> {
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
        "if this now recovers on its own, ratatui changed its swap/flush order and the SCREEN_DIRTY handling in `run` can be simplified"
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
/// draining. On a terminal where the `EventStream` wins that race the read times
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
