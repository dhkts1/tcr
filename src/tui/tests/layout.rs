//! Tests of how the frame shares its height and which accounts layout a width gets.

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
    assert_eq!(accounts_title(7, 7), " tcr · accounts (7) ");
}

#[test]
fn accounts_title_flags_clip() {
    assert_eq!(accounts_title(5, 7), " tcr · accounts (5/7 ▼) ");
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
