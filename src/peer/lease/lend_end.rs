//! `--for` / `--until` as a lease end: the parsing [`super::parse_lend_end`] does.

use anyhow::{Context as _, Result};

/// Turn `--for <d>` / `--until <clock>` into [`Lease::until`](tcr_peer_wire::Lease::until):
/// an absolute unix second, or `None` for "no end".
///
/// The clock is INJECTED (`now`) rather than read here, which is what makes the
/// gate a measurement: `--for 2h` is asserted to write exactly two hours past a
/// chosen instant, which a function reading the wall clock could only be
/// asserted about approximately.
///
/// Three spellings, and nothing else:
///
/// - `none` or an empty string: no end. Named rather than implied, so an
///   operator can clear an end with the same flag they set it with.
/// - a duration: `30s`, `90m`, `2h`, `3d`, which is `--for`;
/// - a wall clock: `18:00` or `18:00:30`, which is `--until`, resolved in the
///   OPERATOR's local offset (`UtcOffset::local_offset_at`, the same
///   fallback-to-UTC pattern [`crate::peer::config::schedule::Schedule::contains`]
///   uses, since the lookup refuses in a multithreaded process). A clock that
///   has already passed today is REFUSED, never rolled to tomorrow: rolling
///   silently is what let a lease edited through the panel at 18:00 for
///   "tomorrow at 18:00" round-trip as an end later THIS minute instead,
///   because the panel's own refusal (`LeaseDraft.refusal`) and this
///   function disagreed about which clock a bare `18:00` resolves against.
///   The refusal names the time it resolved to, so `--for 24h` or an
///   absolute end is the way forward, not a second bare clock.
///
/// A bare number is REFUSED rather than read as seconds or as an hour: `2` is
/// two hours to one reader and two seconds to another, and a lease is not
/// something to guess a unit on.
pub fn parse_lend_end(spec: &str, now: time::OffsetDateTime) -> Result<Option<u64>> {
    let offset = time::UtcOffset::local_offset_at(now).unwrap_or(time::UtcOffset::UTC);
    parse_lend_end_at_local(spec, now.to_offset(offset))
}

/// [`parse_lend_end`]'s decision, given `local_now` already resolved to the
/// operator's own offset.
///
/// Split out for the reason [`crate::peer::config::schedule::Schedule::contains_at_local`]
/// is: this is what the unit tests below exercise directly, so they assert
/// this function's arithmetic and never the host machine's timezone, and
/// `local_offset_at` is not called a second time somewhere that would refuse
/// it in a multithreaded process.
pub(super) fn parse_lend_end_at_local(
    spec: &str,
    local_now: time::OffsetDateTime,
) -> Result<Option<u64>> {
    let spec = spec.trim();
    if spec.is_empty() || spec.eq_ignore_ascii_case("none") {
        return Ok(None);
    }

    if let Some((hours, minutes, seconds)) = parse_clock(spec) {
        let wanted = time::Time::from_hms(hours, minutes, seconds).with_context(|| {
            format!("peer lend: {spec} is not a time of day (00:00:00 to 23:59:59)")
        })?;
        let at = local_now.replace_time(wanted);
        if at <= local_now {
            anyhow::bail!(
                "peer lend: {spec} has already passed today (it resolved to {}), so that end \
                 is in the past; give a clock time still ahead today, or an absolute end \
                 instead of a bare `--until`",
                at.format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_else(|_| spec.to_string())
            );
        }
        return Ok(Some(u64::try_from(at.unix_timestamp()).unwrap_or(0)));
    }

    let split_at = spec
        .char_indices()
        .next_back()
        .map_or(0, |(byte_index, _)| byte_index);
    let (digits, unit) = spec.split_at(split_at);
    let multiplier = match unit {
        "s" => 1_i64,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        _ => anyhow::bail!(
            "peer lend: {spec:?} is neither a duration (`30s`, `90m`, `2h`, `3d`) nor a time \
             of day (`18:00`); a bare number is refused rather than guessed at"
        ),
    };
    let count: i64 = digits
        .parse()
        .with_context(|| format!("peer lend: {digits:?} in {spec:?} is not a whole number"))?;
    if count <= 0 {
        anyhow::bail!(
            "peer lend: {spec:?} is not a duration into the future; `--for none` is how a \
             lease is lent with no end"
        );
    }
    let offset = count.checked_mul(multiplier).with_context(|| {
        format!("peer lend: {spec:?} is too large a duration to compute an end from")
    })?;
    let end = local_now
        .unix_timestamp()
        .checked_add(offset)
        .with_context(|| {
            format!("peer lend: {spec:?} is too large a duration to compute an end from")
        })?;
    Ok(Some(u64::try_from(end).unwrap_or(0)))
}

/// `HH:MM` or `HH:MM:SS` as its three numbers, or `None` for anything else.
///
/// Split out so [`parse_lend_end_at_local`] reads as the three spellings it
/// accepts rather than as a parser: the interesting decision in that function
/// is refusing a clock already behind local now, and it is invisible inside a
/// `split(':')` chain.
fn parse_clock(spec: &str) -> Option<(u8, u8, u8)> {
    let mut parts = spec.split(':');
    let hours: u8 = parts.next()?.parse().ok()?;
    let minutes: u8 = parts.next()?.parse().ok()?;
    let seconds: u8 = match parts.next() {
        Some(seconds) => seconds.parse().ok()?,
        None => 0,
    };
    if parts.next().is_some() {
        return None;
    }
    Some((hours, minutes, seconds))
}
