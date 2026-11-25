//! The day files: the line format, retention, and closed-day compression.

use super::*;

/// A ledger line carrying a key this build does not read parses through the one real
/// parse path, and the keys it does read survive.
///
/// The ledger outlives the binary that wrote it: a newer tcr writes keys an older one
/// has never heard of, and the older one replays the same files after a downgrade.
/// That works only while `LedgerLine` skips what it does not know, and this is what
/// fails the day it starts rejecting unknown keys. `r` is not the unknown key here: it
/// is the cache-read count every line already carries.
#[test]
fn ledger_line_parses_with_an_unknown_key() {
    let dir = scratch("ledger-unknown-key");
    std::fs::create_dir_all(&dir).expect("create the scratch directory");
    let path = dir.join("2026-01-01.jsonl");
    std::fs::write(
        &path,
        concat!(
            r#"{"t":1767225600000,"a":"bob@example.com/codex","m":"gpt-5.1-codex","i":1,"c5":0,"c1":0,"r":0,"o":2,"p":"codex","addedLater":{"n":1}}"#,
            "\n"
        ),
    )
    .expect("write the ledger line");

    let (lines, malformed) = parse_ledger_day(&dir, time::macros::date!(2026 - 01 - 01));
    assert_eq!(
        malformed, 0,
        "an unknown key does not make a line malformed"
    );
    assert_eq!(lines.len(), 1, "the line is read, not skipped");
    assert_eq!(lines[0].a, "bob@example.com/codex");
    assert_eq!(lines[0].m.as_deref(), Some("gpt-5.1-codex"));
    assert_eq!(lines[0].o, 2);

    std::fs::remove_dir_all(&dir).expect("remove the scratch directory");
}

/// A Codex line says so, with `"p":"codex"`, and adds its reasoning tokens as `rt` when there
/// are any. An Anthropic line carries neither key, so a Claude-only ledger's lines are byte
/// for byte what they were. `rt` is never `r`: that one is the cache-read count.
#[test]
fn the_ledger_writes_p_and_rt_only_for_codex() {
    let dir = scratch("provider-keys");
    let now = crate::now_ms();
    let tracker = tracker();
    tracker.attach_ledger(
        dir.clone(),
        90,
        &[named("alice@example.com"), named("bob@example.com/codex")],
        now,
    );
    let base = |model: &str| {
        UsageRecord::from_quota_input(now, Some(model.to_string()), None, 100, 0, 0, 40, 5)
    };
    let codex = |reasoning: u64| UsageRecord {
        provider: Provider::Codex,
        reasoning,
        ..base("gpt-5.1-codex")
    };
    tracker.record(0, &base("claude-opus-5"), "alice@example.com", None, None);
    tracker.record(1, &codex(7), "bob@example.com/codex", None, None);
    tracker.record(1, &codex(0), "bob@example.com/codex", None, None);
    assert!(tracker.flush_ledger(), "the writer answered the flush");
    let text =
        std::fs::read_to_string(dir.join(format!("{}.jsonl", date_string(ledger_date(now)))))
            .expect("today's file");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3);
    assert_eq!(
        lines[0],
        format!(
            r#"{{"t":{now},"a":"alice@example.com","m":"claude-opus-5","s":null,"i":60,"c5":0,"c1":0,"r":40,"o":5}}"#
        ),
        "an Anthropic line is the line written before this change"
    );
    let reasoned: serde_json::Value = serde_json::from_str(lines[1]).expect("a JSON line");
    assert_eq!(
        (
            reasoned["p"].as_str(),
            reasoned["rt"].as_u64(),
            reasoned["r"].as_u64()
        ),
        (Some("codex"), Some(7), Some(40))
    );
    let quiet: serde_json::Value = serde_json::from_str(lines[2]).expect("a JSON line");
    assert_eq!(quiet["p"], "codex");
    assert!(quiet.get("rt").is_none(), "no reasoning, no key");
    std::fs::remove_dir_all(&dir).log_debug("removing a test directory");
}

/// A line written before the ledger knew its provider still parses, as no provider and no
/// reasoning tokens. `p` and `rt` are optional, so every day file on disk stays readable,
/// and an older tcr reads a new line too, since `LedgerLine` refuses no unknown key.
#[test]
fn ledger_lines_without_p_or_rt_still_parse() {
    let dir = scratch("pre-provider");
    std::fs::create_dir_all(&dir).expect("create the scratch directory");
    let path = dir.join("2026-01-01.jsonl");
    std::fs::write(
        &path,
        concat!(
            r#"{"t":1767225600000,"a":"alice@example.com","m":"claude-opus-5","i":1,"c5":0,"c1":0,"r":0,"o":2}"#,
            "\n",
            r#"{"t":1767225600000,"a":"bob@example.com/codex","p":"codex","m":"gpt-5.1-codex","i":1,"c5":0,"c1":0,"r":3,"o":2,"rt":1}"#,
            "\n",
        ),
    )
    .expect("write a day file");
    let (lines, malformed) = parse_ledger_day(&dir, time::macros::date!(2026 - 01 - 01));
    assert_eq!((lines.len(), malformed), (2, 0));
    assert_eq!((lines[0].p.as_deref(), lines[0].rt), (None, 0));
    assert_eq!(
        (lines[1].p.as_deref(), lines[1].rt, lines[1].r),
        (Some("codex"), 1, 3),
        "rt is reasoning; r stays the cache read"
    );
    std::fs::remove_dir_all(&dir).log_debug("removing a test directory");
}

/// FINDING 5. `usageRetentionDays: 0` is the documented FLOOR — today and
/// yesterday, the two files the boot replay reads — not an instruction to
/// delete the day just replayed. On a UTC+3 machine yesterday's UTC file
/// holds the first three hours of the local day.
#[test]
fn retention_never_prunes_today_or_yesterday() {
    let dir = scratch("prune-floor");
    ensure_dir(&dir).expect("temp dir");
    let today = ledger_date(crate::now_ms());
    let yesterday = today - time::Duration::days(1);
    let older = today - time::Duration::days(2);
    let write_days = || {
        for date in [today, yesterday, older] {
            std::fs::write(dir.join(format!("{}.jsonl", date_string(date))), "").expect("write");
        }
    };
    let exists = |date: Date| dir.join(format!("{}.jsonl", date_string(date))).exists();

    write_days();
    assert_eq!(prune(&dir, today, 0), 1, "0 deletes only what is older");
    assert!(exists(today), "0 keeps today");
    assert!(
        exists(yesterday),
        "0 keeps yesterday — the boot replay reads it, and config.rs calls this the floor"
    );
    assert!(!exists(older));

    write_days();
    assert_eq!(prune(&dir, today, 1), 1, "1 is the same floor");
    assert!(exists(today) && exists(yesterday));
    assert!(!exists(older));

    std::fs::remove_dir_all(&dir).log_debug("removing a test directory");
}

/// Retention deletes old day files and leaves everything else in the
/// directory alone.
#[test]
fn prune_deletes_only_old_dated_files() {
    let dir = scratch("prune");
    ensure_dir(&dir).expect("temp dir");
    let today = ledger_date(crate::now_ms());
    let old = today - time::Duration::days(100);
    let recent = today - time::Duration::days(2);
    for date in [old, recent] {
        std::fs::write(dir.join(format!("{}.jsonl", date_string(date))), "").expect("write");
    }
    std::fs::write(dir.join("notes.txt"), "keep me").expect("write");

    assert_eq!(prune(&dir, today, 90), 1);
    assert!(!dir.join(format!("{}.jsonl", date_string(old))).exists());
    assert!(dir.join(format!("{}.jsonl", date_string(recent))).exists());
    assert!(
        dir.join("notes.txt").exists(),
        "unrelated files are untouched"
    );
    std::fs::remove_dir_all(&dir).log_debug("removing a test directory");
}

/// A compressed day ages out like a plain one.
#[test]
fn prune_deletes_old_compressed_days_too() {
    let dir = scratch("prune-gz");
    ensure_dir(&dir).expect("temp dir");
    let today = ledger_date(crate::now_ms());
    let old = today - time::Duration::days(100);
    let recent = today - time::Duration::days(2);
    for date in [old, recent] {
        write_gz(&gz_day_path(&dir, date), b"").expect("write");
    }
    assert_eq!(prune(&dir, today, 90), 1);
    assert!(!gz_day_path(&dir, old).exists());
    assert!(gz_day_path(&dir, recent).exists());
    std::fs::remove_dir_all(&dir).log_debug("removing a test directory");
}

/// One fake ledger line, in the shape the writer serializes.
fn fake_line(t: i64, n: u64) -> String {
    let line = LedgerLine {
        t,
        a: format!("user{}@example.com", n % 18),
        u: Some(format!("{:08}-1111-4111-8111-111111111111", n % 18)),
        g: Some(format!("{:08}-2222-4222-8222-222222222222", n % 18)),
        p: None,
        m: Some(
            if n.is_multiple_of(3) {
                "claude-opus-4-1"
            } else {
                "claude-sonnet-4-5"
            }
            .to_string(),
        ),
        s: Some(n.wrapping_mul(0x9e37_79b9_7f4a_7c15) % 4_000),
        i: n % 7,
        c5: (n * 131) % 9_000,
        c1: (n * 17) % 40_000,
        r: 150_000 + (n * 7_919) % 200_000,
        o: (n * 37) % 3_000,
        rt: 0,
    };
    let mut text = serde_json::to_string(&line).expect("serialize");
    text.push('\n');
    text
}

/// The closed-day compression: every day before yesterday becomes a `.gz` holding exactly
/// its bytes, today's and yesterday's stay plain, and every reader reads the same lines
/// either way. The two two-file states are covered: a leftover plain file (the `.gz`
/// already ends with it) is removed and read once, and a plain file of late lines is
/// folded in after the `.gz`.
#[test]
fn closed_days_are_gzipped_and_read_back_identical() {
    let dir = scratch("compress");
    ensure_dir(&dir).expect("temp dir");
    let today = ledger_date(crate::now_ms());
    let day = |back: i64| today - time::Duration::days(back);
    let text = |back: i64, lines: u64| -> String {
        (0..lines)
            .map(|n| fake_line(1_700_000_000_000 + back * 1_000 + n as i64, n))
            .collect()
    };
    for back in [0, 1, 2, 5] {
        std::fs::write(plain_day_path(&dir, day(back)), text(back, 50)).expect("write");
    }
    // Six days back: compressed earlier, then two late lines reopened its plain file.
    write_gz(&gz_day_path(&dir, day(6)), text(6, 30).as_bytes()).expect("write gz");
    std::fs::write(
        plain_day_path(&dir, day(6)),
        fake_line(1, 1) + &fake_line(2, 2),
    )
    .expect("write late lines");
    // Seven days back: a pass that stopped between its rename and its remove.
    write_gz(&gz_day_path(&dir, day(7)), text(7, 30).as_bytes()).expect("write gz");
    let tail: String = text(7, 30)
        .lines()
        .skip(20)
        .map(|l| format!("{l}\n"))
        .collect();
    std::fs::write(plain_day_path(&dir, day(7)), tail).expect("write leftover");

    let before: Vec<String> = [0, 1, 2, 5, 6, 7]
        .iter()
        .map(|&back| read_ledger_day(&dir, day(back)))
        .collect();
    assert_eq!(before[5], text(7, 30), "a leftover is read once");
    assert_eq!(
        before[4],
        text(6, 30) + &fake_line(1, 1) + &fake_line(2, 2),
        "late lines are read after the compressed day"
    );

    let mut between = 0;
    let report = compress_closed_days(&dir, today, || between += 1);
    assert_eq!(report.days, 4, "{report:?}");
    assert_eq!(report.failed, 0, "{report:?}");
    assert_eq!(between, 4, "the queue is answered after every day");
    for back in [0, 1] {
        assert!(
            plain_day_path(&dir, day(back)).exists(),
            "day -{back} stays plain"
        );
        assert!(
            !gz_day_path(&dir, day(back)).exists(),
            "day -{back} is not compressed"
        );
    }
    for back in [2, 5, 6, 7] {
        assert!(
            !plain_day_path(&dir, day(back)).exists(),
            "day -{back} plain removed"
        );
        assert!(
            gz_day_path(&dir, day(back)).exists(),
            "day -{back} compressed"
        );
    }
    let after: Vec<String> = [0, 1, 2, 5, 6, 7]
        .iter()
        .map(|&back| read_ledger_day(&dir, day(back)))
        .collect();
    assert_eq!(before, after, "every day reads back identical");
    let (lines, malformed) = parse_ledger_day(&dir, day(5));
    assert_eq!((lines.len(), malformed), (50, 0));
    assert!(
        std::fs::read_dir(&dir)
            .expect("list")
            .flatten()
            .all(|e| !e.file_name().to_string_lossy().ends_with(".tmp")),
        "no temporary file is left behind"
    );

    // A second pass finds nothing left to do.
    assert_eq!(compress_closed_days(&dir, today, || {}).days, 0);
    std::fs::remove_dir_all(&dir).log_debug("removing a test directory");
}

/// The size a compressed day saves, on a generated day of ledger lines in the writer's
/// own shape at about a busy day's count. Ignored by default (it writes about 30 MB);
/// `cargo test --lib -- --ignored --nocapture ledger_day_compression` prints the sizes.
#[test]
#[ignore = "writes a full-size day to measure the compression"]
fn ledger_day_compression_on_a_generated_day() {
    let dir = scratch("compress-size");
    ensure_dir(&dir).expect("temp dir");
    let today = ledger_date(crate::now_ms());
    let day = today - time::Duration::days(3);
    let start = 1_700_000_000_000_i64;
    let lines: u64 = 180_000;
    let mut text = String::new();
    for n in 0..lines {
        text.push_str(&fake_line(start + (n as i64) * 480, n));
    }
    std::fs::write(plain_day_path(&dir, day), &text).expect("write");
    let started = std::time::Instant::now();
    let report = compress_closed_days(&dir, today, || {});
    let took = started.elapsed();
    assert_eq!(report.days, 1);
    assert_eq!(read_ledger_day(&dir, day), text);
    eprintln!(
        "ledger day: {lines} lines, plain {} bytes, gz {} bytes ({:.1}%), compressed in {took:?}",
        report.plain_bytes,
        report.gz_bytes,
        report.gz_bytes as f64 / report.plain_bytes as f64 * 100.0
    );
    std::fs::remove_dir_all(&dir).log_debug("removing a test directory");
}
