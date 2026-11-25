use super::lend_end::parse_lend_end_at_local;
use super::*;
use time::macros::datetime;

/// A fixed LOCAL reading, `+02:00`, chosen off the machine this suite
/// happens to run on: 2023-11-14 22:13:20 local. Every clock-time
/// assertion below goes through [`parse_lend_end_at_local`] rather than
/// [`parse_lend_end`] for the reason
/// [`crate::peer::config::schedule::Schedule::contains_at_local`]'s own tests do
/// the same split: `UtcOffset::local_offset_at` is real and
/// host-dependent, and a test asserting against it would pass or fail
/// depending on which machine ran the suite rather than on the code.
fn local_now() -> time::OffsetDateTime {
    datetime!(2023-11-14 22:13:20 +02:00)
}

#[test]
fn a_clock_still_ahead_today_resolves_to_today() {
    let local = local_now();
    let end = parse_lend_end_at_local("23:00", local)
        .expect("23:00 parses")
        .expect("a clock time is an end");
    let at = time::OffsetDateTime::from_unix_timestamp(i64::try_from(end).expect("in range"))
        .expect("a valid instant")
        .to_offset(local.offset());
    assert_eq!((at.hour(), at.minute()), (23, 0));
    assert_eq!(at.date(), local.date(), "later today stays today");
}

#[test]
fn a_clock_already_passed_today_is_refused_not_rolled_to_tomorrow() {
    let local = local_now(); // 22:13:20
    let err = parse_lend_end_at_local("09:00", local).expect_err("a past clock must be refused");
    let message = err.to_string();
    assert!(
        message.contains("09:00"),
        "names the spelling refused: {message}"
    );
    assert!(
        message.contains("already passed") && message.contains("past"),
        "says the end is in the past rather than silently rolling: {message}"
    );
}

#[test]
fn a_duration_and_none_are_resolved_against_the_same_instant_regardless_of_offset() {
    let local = local_now();
    assert_eq!(
        parse_lend_end_at_local("2h", local).expect("2h parses"),
        Some(u64::try_from(local.unix_timestamp() + 7_200).expect("positive"))
    );
    assert_eq!(
        parse_lend_end_at_local("none", local).expect("none parses"),
        None
    );
    assert_eq!(
        parse_lend_end_at_local("", local).expect("empty parses"),
        None
    );
}

#[test]
fn a_bare_number_is_still_refused() {
    for refused in ["2", "2w", "0h", "-1h", "25:00", "18:60", "18:00:00:00"] {
        assert!(
            parse_lend_end_at_local(refused, local_now()).is_err(),
            "{refused:?} is not an end and must be refused rather than guessed at"
        );
    }
}

/// A smoke test on the public wrapper: it must actually convert `now`
/// into SOME offset and delegate rather than panic, on the one input
/// (`none`) whose answer cannot depend on which offset that was.
#[test]
fn the_public_wrapper_resolves_an_offset_and_delegates() {
    let now = time::OffsetDateTime::from_unix_timestamp(1_700_000_000)
        .expect("a literal unix timestamp is a valid instant");
    assert_eq!(parse_lend_end("none", now).expect("none parses"), None);
}

/// `split_at(spec.len() - 1)` splits by byte, not by character: a
/// trailing multibyte character (`--for 2é`) lands mid-character and
/// panics rather than being refused. `é` is not a real duration unit
/// either way, so this must be a clean refusal, never a panic.
#[test]
fn a_multibyte_trailing_unit_is_refused_not_a_panic() {
    let err = parse_lend_end_at_local("2é", local_now())
        .expect_err("a multibyte unit is not a duration and must be refused");
    assert!(
        err.to_string().contains("2é"),
        "names the spelling refused: {err}"
    );
}

/// `count * multiplier` overflowing used to go through `unwrap_or(0)`
/// and hand back `until = 0`: a lease dead on arrival, with no sign
/// anything went wrong. An overflow must be a refusal an operator can
/// read instead.
#[test]
fn an_overflowing_duration_is_refused_not_a_dead_on_arrival_zero() {
    let err = parse_lend_end_at_local(&format!("{}d", i64::MAX), local_now())
        .expect_err("a duration whose arithmetic overflows must be refused");
    assert!(
        err.to_string().contains("too large"),
        "names why the duration was refused: {err}"
    );
}
