//! The questions callers outside this module ask a [`Quota`], answered by its own methods.

use super::*;

fn window(utilization: f64, reset: Option<OffsetDateTime>) -> QuotaWindow {
    QuotaWindow { utilization, reset }
}

/// Each bucket reads live: its own window, a past reset as a fresh `0.0` with no reset, and
/// an unread window as `None`.
#[test]
fn a_bucket_reads_its_own_window_live() {
    let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
    let ahead = now + Duration::hours(1);
    let quota = Quota {
        five_hour: Some(window(0.4, Some(ahead))),
        seven_day: Some(window(0.9, Some(now - Duration::seconds(1)))),
        seven_day_oi: Some(window(0.7, Some(ahead + Duration::days(1)))),
        ..Quota::default()
    };
    assert_eq!(quota.effective(QuotaBucket::FiveHour, now), Some(0.4));
    assert_eq!(quota.live_reset(QuotaBucket::FiveHour, now), Some(ahead));
    assert_eq!(quota.effective(QuotaBucket::SevenDay, now), Some(0.0));
    assert_eq!(quota.live_reset(QuotaBucket::SevenDay, now), None);
    assert_eq!(quota.effective(QuotaBucket::SevenDayOi, now), Some(0.7));
    assert_eq!(
        quota.live_reset(QuotaBucket::SevenDayOi, now),
        Some(ahead + Duration::days(1))
    );
    let unread = Quota::default();
    assert_eq!(unread.effective(QuotaBucket::FiveHour, now), None);
    assert_eq!(unread.live_reset(QuotaBucket::SevenDayOi, now), None);
}

/// A Claude bank is spendable only while the last read called it eligible; Codex credits only
/// while one is held.
#[test]
fn banked_resets_are_offered_only_while_they_can_be_spent() {
    let bank = |eligible| crate::claude_resets::ClaudeResetBank {
        eligible,
        reason: None,
        grants: Vec::new(),
    };
    let credits = |available_count| tcr_status_wire::ResetCredits {
        available_count,
        earliest_expiry_ms: Some(1_900_000_000_000),
    };
    let mut quota = Quota::default();
    assert!(quota.eligible_claude_resets().is_none(), "never read");
    assert!(
        quota.spendable_codex_reset_credits().is_none(),
        "never read"
    );
    quota.claude_resets = Some(bank(false));
    quota.codex.reset_credits = Some(credits(0));
    assert!(quota.eligible_claude_resets().is_none());
    assert!(quota.spendable_codex_reset_credits().is_none());
    quota.claude_resets = Some(bank(true));
    quota.codex.reset_credits = Some(credits(2));
    assert_eq!(quota.eligible_claude_resets(), Some(&bank(true)));
    assert_eq!(quota.spendable_codex_reset_credits(), Some(&credits(2)));
}

/// A refusal's own deadline is ahead strictly before it, passed from the instant it arrives,
/// and unnamed when the refusal carried none.
#[test]
fn a_refusal_deadline_is_ahead_passed_or_unnamed() {
    let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
    let mut quota = Quota::default();
    assert_eq!(quota.codex_refusal_deadline(now), RefusalDeadline::Unnamed);
    let reset = now + Duration::minutes(10);
    quota.codex.refusal_reset = Some(reset);
    assert_eq!(
        quota.codex_refusal_deadline(now),
        RefusalDeadline::Ahead(reset)
    );
    assert_eq!(quota.codex_refusal_deadline(reset), RefusalDeadline::Passed);
}

/// The header readings count every window the last response carried, and give back only the
/// stored readings of those, so a window it did not carry never reads as its tick.
#[test]
fn header_readings_are_the_carried_windows_only() {
    let reading = window(0.5, None);
    let mut quota = Quota {
        five_hour: Some(reading),
        seven_day: Some(reading),
        ..Quota::default()
    };
    assert!(quota.codex_header_readings().is_none(), "no response yet");
    quota.codex.header_windows = Some(CodexWindows {
        five_hour: None,
        seven_day: Some(CodexWindow::default()),
    });
    let (carried, [five_hour, seven_day]) = quota.codex_header_readings().unwrap();
    assert_eq!(carried, 1);
    assert!(five_hour.is_none(), "not carried, though stored");
    assert_eq!(seven_day.map(|w| w.utilization), Some(0.5));
    // Carried, then cleared by a probe that found the plan has no such window: still counted.
    quota.codex.header_windows = Some(CodexWindows {
        five_hour: Some(CodexWindow::default()),
        seven_day: Some(CodexWindow::default()),
    });
    quota.five_hour = None;
    let (carried, [five_hour, _]) = quota.codex_header_readings().unwrap();
    assert_eq!(carried, 2);
    assert!(five_hour.is_none());
}

/// An Anthropic row reports its banked resets and no Codex fact; a Codex row the reverse.
#[test]
fn wire_facts_report_only_the_rows_own_provider() {
    let mut quota = Quota::seeded(
        None,
        None,
        Some(CodexWindows {
            five_hour: None,
            seven_day: Some(CodexWindow {
                minutes: Some(10_080),
            }),
        }),
    );
    quota.codex.spend_control_reached = true;
    quota.claude_resets = Some(crate::claude_resets::ClaudeResetBank {
        eligible: true,
        reason: None,
        grants: Vec::new(),
    });

    let claude = quota.wire_facts(crate::provider::Provider::Anthropic);
    assert!(claude.banked_resets.is_some());
    assert_eq!(claude.quota_windows, None);
    assert!(!claude.spend_control_reached);

    let codex = quota.wire_facts(crate::provider::Provider::Codex);
    assert!(codex.banked_resets.is_none());
    assert_eq!(codex.quota_windows, Some(vec!["sevenDay".to_string()]));
    assert_eq!(codex.five_hour_window_minutes, None);
    assert_eq!(codex.seven_day_window_minutes, Some(10_080));
    assert!(codex.spend_control_reached);
}
