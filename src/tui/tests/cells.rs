//! Tests of the shared cell formatters: bars, percentages, ages, gate chips.

use super::*;

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
