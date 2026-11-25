//! The small formatters the panes share: quota bars and percentages, probe and gate chips,
//! ages, token counts and status colours.

use ratatui::style::{Color, Modifier, Style};
use time::{Duration as TimeDuration, OffsetDateTime};

use crate::base::provider::Provider;
use crate::records::probe::ProbeStatus;
use crate::records::stats::{AccountSnapshot, GateReason, QuotaState};

/// An 8-cell utilization bar plus a percentage, or a dash if never learned.
pub(super) fn bar(util: Option<f64>) -> String {
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
pub(super) fn bar_with_reset(
    util: Option<f64>,
    reset: Option<OffsetDateTime>,
    now: OffsetDateTime,
) -> String {
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
pub(super) fn pct(util: Option<f64>) -> String {
    match util {
        None => "—".to_string(),
        Some(util) => format!("{:>3}%", (util * 100.0).round() as i64),
    }
}

/// The probe-health cell: an age since the last probe plus a coloured status.
pub(super) fn probe_cell(account: &AccountSnapshot, now: OffsetDateTime) -> (String, Style) {
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
pub(super) fn quota_cell(state: QuotaState) -> (&'static str, Style) {
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
pub(super) fn gate_chip(account: &AccountSnapshot, now: OffsetDateTime) -> (String, Style) {
    match account.provider {
        Provider::Anthropic => {}
        Provider::Codex => {
            if let Some(chip) = codex_gate_chip(account, now) {
                return chip;
            }
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

pub(super) fn status_style(status: &str) -> Style {
    match status {
        "active" => Style::default().fg(Color::Green),
        "throttled" => Style::default().fg(Color::Yellow),
        "error" => Style::default().fg(Color::Red),
        _ => Style::default(),
    }
}

pub(super) const fn status_color(status: u16) -> Color {
    match status {
        200..=299 => Color::Green,
        429 => Color::Magenta,
        400..=499 => Color::Yellow,
        500..=599 => Color::Red,
        _ => Color::Gray,
    }
}

/// Humanize a token count: `1234` → `1.2k`, `2_000_000` → `2.0M`.
pub(super) fn fmt_tokens(count: u64) -> String {
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
pub(super) fn fmt_cache_ratio(cache_read: u64, input_total: u64) -> String {
    if input_total == 0 {
        "-".to_string()
    } else {
        format!("{:.0}%", cache_read as f64 / input_total as f64 * 100.0)
    }
}

/// Age of an optional instant relative to `now`, or an em-dash when absent —
/// the "—"-or-[`fmt_age`] pattern shared by the account, session, and probe cells.
pub(super) fn fmt_age_opt(seen: Option<OffsetDateTime>, now: OffsetDateTime) -> String {
    seen.map_or_else(|| "—".to_string(), |t| fmt_age(now - t))
}

/// Humanize a duration as a compact age (`45s`, `12m`, `3h`, `2d`).
pub(super) fn fmt_age(delta: TimeDuration) -> String {
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
pub(super) fn rel(d: TimeDuration) -> String {
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
pub(super) fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let head: String = text.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}
