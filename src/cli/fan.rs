//! `tcr fan`: read the running proxy's fan controller, send it a mode, and wait for the state to settle.

use std::path::Path;
use std::time::Duration;

use anyhow::bail;

use super::live_client::{
    ControlRoute, LiveControlError, LiveStatusError, call_route, fetch_live_status,
};
use super::load_config_with_creation;
use crate::config::Config;

/// [`crate::proxy::FAN_PATH`]. 400 is a refused mode, 503 a server with no fan controller.
const FAN_ROUTE: ControlRoute = ControlRoute {
    path: crate::proxy::FAN_PATH,
    endpoint: crate::proxy::FAN_ENDPOINT,
    refusals: &[400, 503],
    noun: "fan",
    success_needs_stamp: true,
};

/// How long `tcr fan <mode>` waits for the controller to settle: the version call (up to 15 s,
/// as launchd may start the helper up to 10 s late), then a
/// set-manual per fan at its worst (the unlock, up to 15 s each; two fans on the Macs this
/// was written for), then a set-target per fan. Built from the controller's own deadlines, so
/// a longer one widens this too.
pub const FAN_SETTLE: Duration = Duration::from_secs(
    tcr_fan::controller::VERSION_DEADLINE.as_secs()
        + 2 * tcr_fan::controller::SET_MANUAL_DEADLINE.as_secs()
        + 2 * tcr_fan::controller::SET_TARGET_DEADLINE.as_secs(),
);
const FAN_POLL: Duration = Duration::from_millis(250);

/// Where `tcr fan` stands after one status read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FanWait {
    /// The controller has not acted on the command yet, or is still entering or releasing.
    Waiting,
    /// The controller acted on the command and the state no longer moves.
    Settled,
    /// The controller acted on this command or a later one, and the wish it shows is not this
    /// command's: a later command replaced it.
    Replaced,
    /// The controller's thread stopped (`unavailable / controllerStopped`). It acts on nothing
    /// again and its command number stays where the panic left it, so waiting for this
    /// command to be acted on would only run out the whole [`FAN_SETTLE`].
    Stopped,
}

/// One command as `tcr fan` sent it: what it asked for, and the number the route gave it.
#[derive(Debug, Clone, Copy)]
pub(super) struct FanSent {
    pub(super) request: crate::fan::FanRequest,
    pub(super) command: u64,
}

/// One status read against `sent`. The status pairs each state with the wish it was reached
/// for, under the number of the newest command acted on (`crate::fan`, "What the status
/// shows"): below this command's number, the controller has not acted on it yet; at or past
/// it, the wish shown is this command's, or a later command's that replaced it.
pub(super) fn fan_wait(sent: &FanSent, row: &tcr_status_wire::FanControlRow) -> FanWait {
    // First: a stopped controller never acts on this command, so neither its number nor its
    // wish can move again.
    if row.state == tcr_status_wire::FanStateRow::Unavailable
        && row.reason == Some(tcr_status_wire::FanReasonRow::ControllerStopped)
    {
        return FanWait::Stopped;
    }
    if row.command < sent.command {
        return FanWait::Waiting;
    }
    if row.desired != sent.request.desired_row() {
        return FanWait::Replaced;
    }
    let moving = matches!(
        row.state,
        tcr_status_wire::FanStateRow::Entering | tcr_status_wire::FanStateRow::Releasing
    );
    if moving {
        FanWait::Waiting
    } else {
        FanWait::Settled
    }
}

/// This command's number from the route's 202, or why there is none: a server older than the
/// command counter answered with its whole `fanControl` object, and nothing else it says tells
/// whether it has acted on this command.
pub(super) fn accepted_command(
    accepted: crate::fan::FanAccepted,
    port: u16,
) -> anyhow::Result<u64> {
    accepted.command.ok_or_else(|| {
        anyhow::anyhow!(
            "the tcr on 127.0.0.1:{port} took the fan mode but is older than this tcr's fan \
             command counter, so whether the fans reached it cannot be told; restart that tcr on \
             this build, then run `tcr fan` to read the state"
        )
    })
}

/// Exit 0 only for the mode this command sent, settled in the state it means.
pub(super) fn fan_succeeded(sent: &FanSent, row: &tcr_status_wire::FanControlRow) -> bool {
    fan_wait(sent, row) == FanWait::Settled && row.state == sent.request.settled_state()
}

fn live_status_why(error: &LiveStatusError, port: u16) -> String {
    match error {
        LiveStatusError::NoServer => format!("no tcr is listening on 127.0.0.1:{port}"),
        LiveStatusError::NoAnswer(why) => {
            format!("the tcr on 127.0.0.1:{port} did not answer: {why}")
        }
        LiveStatusError::Unusable(why) => {
            format!("the tcr on 127.0.0.1:{port} answered with something unreadable: {why}")
        }
    }
}

/// The server's fan control and heat, or why there is none to show.
async fn live_fan(
    config: &Config,
) -> anyhow::Result<(
    tcr_status_wire::FanControlRow,
    Option<tcr_status_wire::HeatRow>,
)> {
    let payload = fetch_live_status(config)
        .await
        .map_err(|e| anyhow::anyhow!(live_status_why(&e, config.proxy.port)))?;
    let heat = payload.heat_reading().ok().cloned();
    match payload.fan_control_reading() {
        Ok(row) => Ok((row.clone(), heat)),
        Err(crate::status::Unread::Unparsed(why)) => bail!(
            "the tcr on 127.0.0.1:{} answered with a fanControl object this tcr cannot read \
             ({why}); a server of another build answers this way, so restart it on this build",
            config.proxy.port
        ),
        Err(crate::status::Unread::NotSent) => bail!(
            "the tcr on 127.0.0.1:{} runs no fan controller (an older tcr, or one not started as the server)",
            config.proxy.port
        ),
    }
}

/// `tcr fan`, all of it. With no `request`, print the state; with one, send it, wait for the
/// state to settle, and say how it ended. Never optimistic: exit 0 only when the server
/// reports the state the request means (holding for a manual mode, off for automatic), for
/// the very mode it sent, seen after the command was sent ([`fan_succeeded`]).
pub async fn fan(
    config_path: &Path,
    request: Option<crate::fan::FanRequest>,
    json: bool,
) -> anyhow::Result<i32> {
    let (config, _created) = load_config_with_creation(config_path)?;
    let Some(request) = request else {
        let (row, heat) = live_fan(&config).await?;
        if json {
            println!("{}", serde_json::to_string(&row)?);
        } else {
            print!("{}", render_fan_text(&row, heat.as_ref()));
        }
        return Ok(0);
    };
    // Refused here first, in the route's own words, so a typo costs no round trip.
    request
        .command()
        .map_err(|e| anyhow::anyhow!("the fan mode was refused: {e}"))?;

    let answer: Result<crate::fan::FanAccepted, LiveControlError> =
        call_route(&config, &FAN_ROUTE, |client, url| {
            client.post(url).json(&request)
        })
        .await
        .result;
    let sent = match answer {
        Ok(accepted) => FanSent {
            request,
            command: accepted_command(accepted, config.proxy.port)?,
        },
        Err(LiveControlError::Rejected(message)) => bail!("{message}"),
        Err(LiveControlError::NoRoute) => bail!(
            "the tcr on 127.0.0.1:{} is too old to control fans (no {} route)",
            config.proxy.port,
            crate::proxy::FAN_PATH
        ),
        Err(other) => bail!("could not reach the fan controller: {}", other.why()),
    };

    let started = std::time::Instant::now();
    let (row, heat) = loop {
        let (row, heat) = live_fan(&config).await?;
        if fan_wait(&sent, &row) != FanWait::Waiting || started.elapsed() >= FAN_SETTLE {
            break (row, heat);
        }
        tokio::time::sleep(FAN_POLL).await;
    };
    if json {
        println!("{}", serde_json::to_string(&row)?);
    } else {
        print!("{}", render_fan_text(&row, heat.as_ref()));
    }
    if fan_succeeded(&sent, &row) {
        return Ok(0);
    }
    match fan_wait(&sent, &row) {
        FanWait::Replaced => {
            eprintln!("tcr: another command changed the fan mode before this one settled");
            return Ok(1);
        }
        FanWait::Waiting if row.command < sent.command => {
            eprintln!(
                "tcr: the fan controller had not acted on this command after {} s",
                FAN_SETTLE.as_secs()
            );
            return Ok(1);
        }
        // A stopped controller says so in the line below: its reason and detail.
        FanWait::Waiting | FanWait::Settled | FanWait::Stopped => {}
    }
    let reason = row
        .reason
        .map_or_default(|r| format!(" ({})", fan_word(&r)));
    let detail = row.detail.map_or_default(|d| format!(": {d}"));
    eprintln!(
        "tcr: the fans are {}{reason}{detail}, not {}",
        fan_word(&row.state),
        fan_word(&request.settled_state()),
    );
    Ok(1)
}

/// A wire enum's own JSON spelling, for a line a person reads and a script greps: a bare
/// string, or the one key of serde's tagged form (`{"modeLocked": 1}` is `modeLocked`).
pub(super) fn fan_word<T: serde::Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(word)) => word,
        Ok(serde_json::Value::Object(map)) if map.len() == 1 => {
            let Some((word, _)) = map.into_iter().next() else {
                return "?".to_string();
            };
            word
        }
        // A shape with no word; none of the enums this spells has one.
        _ => "?".to_string(),
    }
}

/// `tcr fan`'s text: the wish, the state and why, then each fan as heat reads it.
pub(super) fn render_fan_text(
    row: &tcr_status_wire::FanControlRow,
    heat: Option<&tcr_status_wire::HeatRow>,
) -> String {
    use tcr_status_wire::FanDesiredRow;
    let desired = match row.desired {
        FanDesiredRow::Auto => "auto".to_string(),
        FanDesiredRow::Fixed { rpm } => format!("fixed {rpm} rpm"),
        FanDesiredRow::FollowCpu {
            quiet_below_c,
            full_at_c,
        } => format!("follow cpu {quiet_below_c}-{full_at_c} °C"),
    };
    let mut out = format!(
        "desired={desired}\nstate={}\nreason={}\n",
        fan_word(&row.state),
        row.reason.map_or_else(|| "-".to_string(), |r| fan_word(&r)),
    );
    if let Some(detail) = &row.detail {
        out.push_str(&format!("detail={detail}\n"));
    }
    out.push_str(&format!(
        "hotGuard={}\n",
        if row.hot_guard { "on" } else { "off" }
    ));
    let Some(heat) = heat else {
        out.push_str("fans=unread (no heat reading from the server)\n");
        return out;
    };
    if let Some(cpu) = heat.cpu_celsius {
        out.push_str(&format!("cpu={cpu:.1}°C\n"));
    }
    for fan in &heat.fans {
        let written = row
            .targets
            .iter()
            .find(|t| t.fan == fan.index)
            .map_or_default(|t| format!(" written={:.0}", t.rpm));
        out.push_str(&format!(
            "fan={} actual={:.0} min={:.0} max={:.0} mode={} target={}{written}\n",
            fan.index,
            fan.actual_rpm,
            fan.min_rpm,
            fan.max_rpm,
            if fan.manual { "manual" } else { "auto" },
            fan.target_rpm
                .map_or_else(|| "-".to_string(), |t| format!("{t:.0}")),
        ));
    }
    out.push_str(&format!(
        "ftst={}\n",
        match heat.ftst {
            Some(true) => "1",
            Some(false) => "0",
            None => "-",
        }
    ));
    out
}
