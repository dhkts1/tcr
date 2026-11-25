//! Tests for `tcr fan` in `cli/fan.rs`: the status text and the wait for the controller
//! to settle.

use super::*;
/// `tcr fan`'s text names the wish, the state, each fan's mode key and target, and
/// `Ftst`: what Gil's hardware checks read.
#[test]
fn fan_status_text_names_every_fact_the_hardware_checks_read() {
    let row = tcr_status_wire::FanControlRow {
        desired: tcr_status_wire::FanDesiredRow::Fixed { rpm: 5000 },
        state: tcr_status_wire::FanStateRow::Foreign,
        reason: Some(tcr_status_wire::FanReasonRow::MacsFanControl),
        hot_guard: false,
        targets: vec![],
        detail: None,
        command: 0,
        since_ms: None,
    };
    let heat = tcr_status_wire::HeatRow {
        cpu_celsius: Some(61.0),
        fans: vec![tcr_status_wire::FanRow {
            index: 0,
            actual_rpm: 5013.0,
            min_rpm: 1350.0,
            max_rpm: 5777.0,
            manual: true,
            target_rpm: Some(5000.0),
        }],
        ftst: Some(false),
    };
    assert_eq!(
        render_fan_text(&row, Some(&heat)),
        "desired=fixed 5000 rpm\nstate=foreign\nreason=macsFanControl\nhotGuard=off\n\
             cpu=61.0°C\nfan=0 actual=5013 min=1350 max=5777 mode=manual target=5000\nftst=0\n"
    );
}

/// A reason's word is its wire name without its data, for the ones that carry data too:
/// what `reason=` and the failure line print.
#[test]
fn fan_reason_word_is_the_wire_name_without_its_data() {
    use tcr_status_wire::FanReasonRow;
    for (reason, word) in [
        (FanReasonRow::MacsFanControl, "macsFanControl"),
        (FanReasonRow::AnotherApp, "anotherApp"),
        (FanReasonRow::NotSignedForFans, "notSignedForFans"),
        (FanReasonRow::NoSmc, "noSmc"),
        (FanReasonRow::NoFans, "noFans"),
        (FanReasonRow::HelperUnreachable, "helperUnreachable"),
        (FanReasonRow::HelperNoReply, "helperNoReply"),
        (FanReasonRow::HelperSignature, "helperSignature"),
        (
            FanReasonRow::HelperVersion { installed: None },
            "helperVersion",
        ),
        (
            FanReasonRow::HelperVersion { installed: Some(2) },
            "helperVersion",
        ),
        (FanReasonRow::ModeLocked(1), "modeLocked"),
        (FanReasonRow::Contended, "contended"),
        (FanReasonRow::HelperRefused, "helperRefused"),
        (FanReasonRow::ControllerStopped, "controllerStopped"),
    ] {
        assert_eq!(fan_word(&reason), word, "{reason:?}");
    }
}

/// A status row for the `tcr fan` tests, under `command`, the newest command acted on.
fn fan_row(
    desired: tcr_status_wire::FanDesiredRow,
    state: tcr_status_wire::FanStateRow,
    command: u64,
) -> tcr_status_wire::FanControlRow {
    tcr_status_wire::FanControlRow {
        desired,
        state,
        reason: None,
        hot_guard: false,
        targets: vec![],
        detail: None,
        command,
        since_ms: None,
    }
}

/// `tcr fan fixed 5000` succeeds only for fixed 5000 itself: holding another mode, even
/// one newer than the command, is not its success.
#[test]
fn fan_cli_succeeds_only_for_the_mode_it_sent() {
    use crate::stores::fan::FanRequest;
    use tcr_status_wire::{FanDesiredRow, FanStateRow};
    let sent = FanSent {
        request: FanRequest::Fixed { rpm: 5000 },
        command: 1_000,
    };
    let ours = FanDesiredRow::Fixed { rpm: 5000 };
    let other = FanDesiredRow::Fixed { rpm: 3000 };

    let settled = fan_row(ours, FanStateRow::Holding, 1_200);
    assert_eq!(fan_wait(&sent, &settled), FanWait::Settled);
    assert!(fan_succeeded(&sent, &settled));

    let replaced = fan_row(other, FanStateRow::Holding, 1_200);
    assert_eq!(fan_wait(&sent, &replaced), FanWait::Replaced);
    assert!(!fan_succeeded(&sent, &replaced));

    // Not acted on yet, or still entering: wait.
    let before = fan_row(ours, FanStateRow::Holding, 900);
    assert_eq!(fan_wait(&sent, &before), FanWait::Waiting);
    assert!(!fan_succeeded(&sent, &before));
    let entering = fan_row(ours, FanStateRow::Entering, 1_200);
    assert_eq!(fan_wait(&sent, &entering), FanWait::Waiting);

    // Settled, but not holding.
    let foreign = fan_row(ours, FanStateRow::Foreign, 1_200);
    assert_eq!(fan_wait(&sent, &foreign), FanWait::Settled);
    assert!(!fan_succeeded(&sent, &foreign));

    // `auto --force` is Auto.
    let forced = FanSent {
        request: FanRequest::Auto { force: true },
        command: 1_000,
    };
    let off = fan_row(FanDesiredRow::Auto, FanStateRow::Off, 1_200);
    assert!(fan_succeeded(&forced, &off));
}

/// The case the command counter exists for (Gil, 2026-10-19): a later command that asks
/// for the very wish from before this one is a replacement, said at once, never a wait for
/// this command that runs out at 49 s.
#[test]
fn fan_cli_a_return_to_the_wish_from_before_is_replaced_at_once() {
    use crate::stores::fan::FanRequest;
    use tcr_status_wire::{FanDesiredRow, FanStateRow};
    let sent = FanSent {
        request: FanRequest::Fixed { rpm: 5000 },
        command: 1_000,
    };
    // Before: Auto, as acted on under 999. Then `auto` again, number 1_001, acted on.
    let before = fan_row(FanDesiredRow::Auto, FanStateRow::Off, 999);
    assert_eq!(fan_wait(&sent, &before), FanWait::Waiting);
    let auto_again = fan_row(FanDesiredRow::Auto, FanStateRow::Off, 1_001);
    assert_eq!(fan_wait(&sent, &auto_again), FanWait::Replaced);
    assert!(!fan_succeeded(&sent, &auto_again));
}

/// Two commands drained in one step are acted on under the later number: the earlier one
/// is judged by the wish shown, its own (settled by the state) or the later one's
/// (replaced).
#[test]
fn fan_cli_a_drained_pair_settles_the_earlier_command_by_its_wish() {
    use crate::stores::fan::FanRequest;
    use tcr_status_wire::{FanDesiredRow, FanStateRow};
    let earlier = FanSent {
        request: FanRequest::Fixed { rpm: 5000 },
        command: 1_000,
    };
    let same = fan_row(
        FanDesiredRow::Fixed { rpm: 5000 },
        FanStateRow::Holding,
        1_001,
    );
    assert_eq!(fan_wait(&earlier, &same), FanWait::Settled);
    assert!(fan_succeeded(&earlier, &same));
    let other = fan_row(
        FanDesiredRow::Fixed { rpm: 3000 },
        FanStateRow::Holding,
        1_001,
    );
    assert_eq!(fan_wait(&earlier, &other), FanWait::Replaced);
}

/// A server restarted after the command went in numbers from its own start, above the
/// command's number, so the command reads as acted on; the wish the restart reapplied (the
/// one the command saved before the restart) decides.
#[test]
fn fan_cli_a_restarted_server_settles() {
    use crate::stores::fan::FanRequest;
    use tcr_status_wire::{FanDesiredRow, FanStateRow};
    let sent = FanSent {
        request: FanRequest::Fixed { rpm: 5000 },
        command: 1_760_000_000_001,
    };
    let restarted = 1_760_000_090_000;
    let held = fan_row(
        FanDesiredRow::Fixed { rpm: 5000 },
        FanStateRow::Holding,
        restarted,
    );
    assert_eq!(fan_wait(&sent, &held), FanWait::Settled);
    assert!(fan_succeeded(&sent, &held));
    let reentering = fan_row(
        FanDesiredRow::Fixed { rpm: 5000 },
        FanStateRow::Entering,
        restarted,
    );
    assert_eq!(fan_wait(&sent, &reentering), FanWait::Waiting);
}

/// A server older than the counter answers the POST with its whole `fanControl` object and
/// writes no `command` in its status. `tcr fan` says so and fails, and reads no other
/// field in its place.
#[test]
fn fan_cli_a_server_without_the_counter_says_so() {
    let old_answer = r#"{"desired":{"mode":"auto"},"state":"off","reason":null,
            "hotGuard":false,"targets":[],"detail":null,"sinceMs":1760000000000}"#;
    let accepted: crate::stores::fan::FanAccepted = serde_json::from_str(old_answer).unwrap();
    let refused = accepted_command(accepted, 3456).unwrap_err().to_string();
    assert!(
        refused.contains("older than this tcr's fan command counter"),
        "{refused}"
    );
    assert!(refused.contains("127.0.0.1:3456"), "{refused}");

    let ours: crate::stores::fan::FanAccepted = serde_json::from_str(r#"{"command":7}"#).unwrap();
    assert_eq!(accepted_command(ours, 3456).unwrap(), 7);
    // This build's 202: the whole object from before the command, with its number.
    let full = r#"{"desired":{"mode":"auto"},"state":"off","reason":null,
            "hotGuard":false,"targets":[],"detail":null,"command":8,"sinceMs":1760000000000}"#;
    let ours: crate::stores::fan::FanAccepted = serde_json::from_str(full).unwrap();
    assert_eq!(accepted_command(ours, 3456).unwrap(), 8);

    // Its status object does not read as one, so nothing is judged from it.
    assert!(serde_json::from_str::<tcr_status_wire::FanControlRow>(old_answer).is_err());
}

/// `tcr fan auto` with the fans held at 5000. The status keeps the old wish and the old
/// number until the controller acts, so the snapshot from taking the command, and a state
/// the old wish reached after the send (the SMC not reading: unknown, still under the old
/// number), both wait; releasing waits; `off` succeeds. A third wish is Replaced.
#[test]
fn fan_cli_auto_from_holding_waits_through_the_release() {
    use crate::stores::fan::FanRequest;
    use tcr_status_wire::{FanDesiredRow, FanStateRow};
    let held = FanDesiredRow::Fixed { rpm: 5000 };
    let sent = FanSent {
        request: FanRequest::Auto { force: false },
        command: 1_000,
    };
    let taken = fan_row(held, FanStateRow::Holding, 999);
    assert_eq!(fan_wait(&sent, &taken), FanWait::Waiting);
    assert!(!fan_succeeded(&sent, &taken));
    let old_wish_moved = fan_row(held, FanStateRow::Unknown, 999);
    assert_eq!(fan_wait(&sent, &old_wish_moved), FanWait::Waiting);
    let releasing = fan_row(FanDesiredRow::Auto, FanStateRow::Releasing, 1_000);
    assert_eq!(fan_wait(&sent, &releasing), FanWait::Waiting);
    let off = fan_row(FanDesiredRow::Auto, FanStateRow::Off, 1_000);
    assert_eq!(fan_wait(&sent, &off), FanWait::Settled);
    assert!(fan_succeeded(&sent, &off));

    let replaced = fan_row(
        FanDesiredRow::Fixed { rpm: 3000 },
        FanStateRow::Holding,
        1_200,
    );
    assert_eq!(fan_wait(&sent, &replaced), FanWait::Replaced);
    assert!(!fan_succeeded(&sent, &replaced));
}

/// A controller that stopped on a panic keeps the command number it had, below this
/// command's, and never acts again: `tcr fan` ends its wait at the first status that says
/// so, whatever the wish and number shown, and fails with that reason.
#[test]
fn fan_cli_a_stopped_controller_ends_the_wait_at_once() {
    use crate::stores::fan::FanRequest;
    use tcr_status_wire::{FanDesiredRow, FanReasonRow, FanStateRow};
    for request in [
        FanRequest::Fixed { rpm: 5000 },
        FanRequest::Auto { force: false },
    ] {
        let sent = FanSent {
            request,
            command: 1_000,
        };
        for (desired, command) in [
            (FanDesiredRow::Auto, 999),
            (FanDesiredRow::Fixed { rpm: 3000 }, 999),
            (request.desired_row(), 1_000),
        ] {
            let mut stopped = fan_row(desired, FanStateRow::Unavailable, command);
            stopped.reason = Some(FanReasonRow::ControllerStopped);
            assert_eq!(
                fan_wait(&sent, &stopped),
                FanWait::Stopped,
                "{request:?} {desired:?} {command}"
            );
            assert!(!fan_succeeded(&sent, &stopped));
        }
    }
    // Another reason under an old number is still a wait: the controller may yet act.
    let sent = FanSent {
        request: FanRequest::Fixed { rpm: 5000 },
        command: 1_000,
    };
    let mut silent = fan_row(FanDesiredRow::Auto, FanStateRow::Unavailable, 999);
    silent.reason = Some(FanReasonRow::HelperNoReply);
    assert_eq!(fan_wait(&sent, &silent), FanWait::Waiting);
}

/// The wait covers the version call (which waits out launchd starting the helper late),
/// two worst-case set-manuals and two set-targets.
#[test]
fn fan_cli_waits_through_two_worst_case_set_manuals() {
    use tcr_fan::controller::{SET_MANUAL_DEADLINE, SET_TARGET_DEADLINE, VERSION_DEADLINE};
    assert!(FAN_SETTLE >= VERSION_DEADLINE + 2 * SET_MANUAL_DEADLINE + 2 * SET_TARGET_DEADLINE);
    assert_eq!(FAN_SETTLE, Duration::from_secs(49));
}
