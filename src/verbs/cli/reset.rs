//! `tcr reset` and `tcr codex reset`: spend a banked reset through the running proxy, after one confirmation.

use std::path::Path;
use std::time::Duration;

use crate::base::config;
use crate::base::provider::Provider;
use crate::proxy::live_client::{
    ControlRoute, LiveControlError, RouteAnswer, call_route_with_policy,
};

const CODEX_RESET_ROUTE: ControlRoute = ControlRoute {
    path: crate::proxy::CODEX_RESET_PATH,
    endpoint: crate::proxy::CODEX_RESET_ENDPOINT,
    refusals: &[400, 404, 409, 502, 504],
    noun: "reset",
    success_needs_stamp: true,
};

/// Confirm once, then keep one private key for every explicit retry of this attempt.
/// Reads only the configured local proxy; no offline redemption or config write.
///
/// The questions go to stderr and only outcome words to stdout, so a caller that
/// shows stdout shows the result and never a question. `assume_yes` is for a
/// caller that already asked (TcrBar's own dialog): it asks nothing, reads no
/// input, and an unknown outcome ends the run instead of offering a retry.
pub async fn codex_reset(
    config_path: &Path,
    account: &str,
    key: Option<&str>,
    assume_yes: bool,
) -> anyhow::Result<i32> {
    codex_reset_with_io(
        config_path,
        account,
        key,
        assume_yes,
        &mut std::io::stdin().lock(),
        &mut std::io::stdout().lock(),
        &mut std::io::stderr().lock(),
    )
    .await
}

async fn codex_reset_with_io(
    config_path: &Path,
    account: &str,
    key: Option<&str>,
    assume_yes: bool,
    input: &mut impl std::io::BufRead,
    output: &mut impl std::io::Write,
    errors: &mut impl std::io::Write,
) -> anyhow::Result<i32> {
    let confirmed = if assume_yes {
        true
    } else {
        let Ok(confirmed) = reset_consent(
            input,
            errors,
            &format!("Redeem one reset for {account}? [y/N]"),
        ) else {
            writeln!(errors, "Reset failed. Could not read confirmation.")?;
            return Ok(4);
        };
        confirmed
    };
    if !confirmed {
        writeln!(output, "Cancelled.")?;
        return Ok(0);
    }
    let key = match key {
        Some(key) => key.to_owned(),
        None => {
            if let Ok(key) = crate::request_key::generate() {
                key
            } else {
                writeln!(errors, "Reset failed. Could not create an attempt.")?;
                return Ok(4);
            }
        }
    };
    if !crate::manager::valid_reset_key(&key) {
        writeln!(errors, "Reset failed. Invalid attempt key.")?;
        return Ok(4);
    }
    let Ok(config) = config::load(config_path) else {
        writeln!(
            errors,
            "Reset failed. Could not read the proxy configuration."
        )?;
        return Ok(4);
    };
    loop {
        let answer: RouteAnswer<crate::manager::CodexResetResponse> = call_route_with_policy(
            &config,
            &CODEX_RESET_ROUTE,
            reqwest::redirect::Policy::none(),
            |client, url| {
                client
                    .post(url)
                    .timeout(Duration::from_secs(95))
                    .json(&serde_json::json!({"query":account,"idempotencyKey":key}))
            },
        )
        .await;
        match answer.result {
            Ok(response) => {
                writeln!(output, "{}", response.outcome.message())?;
                if !response.usage_refreshed {
                    writeln!(errors, "Usage refresh failed.")?;
                    return Ok(5);
                }
                return Ok(
                    if response.outcome == crate::manager::CodexResetOutcome::NoCredit {
                        6
                    } else {
                        0
                    },
                );
            }
            Err(LiveControlError::NoServer) => {
                writeln!(errors, "No running proxy.")?;
                return Ok(3);
            }
            Err(error) => {
                let unknown = answer.status == Some(504)
                    || matches!(
                        error,
                        LiveControlError::NoAnswer(_) | LiveControlError::Unusable(_)
                    );
                if unknown {
                    writeln!(errors, "Reset outcome unknown.")?;
                    // A caller that passed --yes asks its own question: TcrBar shows
                    // Retry and reruns with the same key.
                    if assume_yes {
                        return Ok(4);
                    }
                    let Ok(retry) = reset_consent(input, errors, "Retry this reset attempt? [y/N]")
                    else {
                        writeln!(errors, "Reset failed. Could not read confirmation.")?;
                        return Ok(4);
                    };
                    if retry {
                        continue;
                    }
                } else {
                    writeln!(errors, "Reset failed. The proxy refused the reset request.")?;
                }
                return Ok(4);
            }
        }
    }
}

const CLAUDE_RESET_ROUTE: ControlRoute = ControlRoute {
    path: crate::proxy::CLAUDE_RESET_PATH,
    endpoint: crate::proxy::CLAUDE_RESET_ENDPOINT,
    refusals: &[400, 403, 404, 409, 502, 504],
    noun: "reset",
    success_needs_stamp: true,
};

/// `tcr reset` and its alias `tcr codex reset`: one banked reset, by the account's provider.
/// A Claude account (named exactly as the config names it) goes to the Claude route with its
/// `kind`; every other query keeps the Codex path it always had, where the proxy matches the
/// name. A kind other than `weekly` on a Codex account is refused here: a Codex reset is a
/// weekly reset.
pub async fn reset(
    config_path: &Path,
    account: &str,
    kind: Option<tcr_status_wire::ResetKind>,
    key: Option<&str>,
    assume_yes: bool,
) -> anyhow::Result<i32> {
    let claude = config::load(config_path)
        .is_ok_and(|config| config.provider_of(account) == Some(Provider::Anthropic));
    if !claude {
        if kind.is_some_and(|kind| kind != tcr_status_wire::ResetKind::Weekly) {
            eprintln!("Reset failed. A Codex reset is a weekly reset.");
            return Ok(4);
        }
        return codex_reset(config_path, account, key, assume_yes).await;
    }
    claude_reset_with_io(
        config_path,
        account,
        kind,
        key,
        assume_yes,
        &mut std::io::stdin().lock(),
        &mut std::io::stdout().lock(),
        &mut std::io::stderr().lock(),
    )
    .await
}

/// [`codex_reset_with_io`]'s Claude twin: the same consent, attempt key, exit codes and
/// retry rule, against [`crate::proxy::CLAUDE_RESET_PATH`]. Every word it prints is fixed
/// (the server's own fixed reasons included); nothing from Anthropic's body reaches it.
#[expect(clippy::too_many_arguments, reason = "the io seams are for tests")]
async fn claude_reset_with_io(
    config_path: &Path,
    account: &str,
    kind: Option<tcr_status_wire::ResetKind>,
    key: Option<&str>,
    assume_yes: bool,
    input: &mut impl std::io::BufRead,
    output: &mut impl std::io::Write,
    errors: &mut impl std::io::Write,
) -> anyhow::Result<i32> {
    let noun = kind.map_or("banked reset", tcr_status_wire::ResetKind::noun);
    if !assume_yes {
        let Ok(confirmed) =
            reset_consent(input, errors, &format!("Use a {noun} on {account}? [y/N]"))
        else {
            writeln!(errors, "Reset failed. Could not read confirmation.")?;
            return Ok(4);
        };
        if !confirmed {
            writeln!(output, "Cancelled.")?;
            return Ok(0);
        }
    }
    let key = match key {
        Some(key) => key.to_owned(),
        None => {
            if let Ok(key) = crate::request_key::generate() {
                key
            } else {
                writeln!(errors, "Reset failed. Could not create an attempt.")?;
                return Ok(4);
            }
        }
    };
    if !crate::records::claude_resets::valid_request_id(&key) {
        writeln!(errors, "Reset failed. Invalid attempt key.")?;
        return Ok(4);
    }
    let Ok(config) = config::load(config_path) else {
        writeln!(
            errors,
            "Reset failed. Could not read the proxy configuration."
        )?;
        return Ok(4);
    };
    loop {
        let mut body = serde_json::json!({"query": account, "idempotencyKey": key});
        if let Some(kind) = kind {
            body["kind"] = serde_json::Value::from(kind.token());
        }
        let answer: RouteAnswer<crate::manager::ClaudeResetResponse> = call_route_with_policy(
            &config,
            &CLAUDE_RESET_ROUTE,
            reqwest::redirect::Policy::none(),
            |client, url| {
                client
                    .post(url)
                    .timeout(Duration::from_secs(95))
                    .json(&body)
            },
        )
        .await;
        match answer.result {
            Ok(response) => {
                writeln!(output, "{}", response.outcome.message())?;
                if !response.usage_refreshed {
                    writeln!(errors, "Usage refresh failed.")?;
                    return Ok(5);
                }
                return Ok(
                    if response.outcome == crate::manager::ClaudeResetOutcome::NoResetLeft {
                        6
                    } else {
                        0
                    },
                );
            }
            Err(LiveControlError::NoServer) => {
                writeln!(errors, "No running proxy.")?;
                return Ok(3);
            }
            Err(LiveControlError::Rejected(message)) if answer.status != Some(504) => {
                writeln!(errors, "Reset failed. {message}")?;
                return Ok(4);
            }
            Err(error) => {
                let unknown = answer.status == Some(504)
                    || matches!(
                        error,
                        LiveControlError::NoAnswer(_) | LiveControlError::Unusable(_)
                    );
                if !unknown {
                    writeln!(errors, "Reset failed. The proxy refused the reset request.")?;
                    return Ok(4);
                }
                writeln!(
                    errors,
                    "{}",
                    crate::manager::ClaudeResetError::Unknown.message()
                )?;
                if assume_yes {
                    return Ok(4);
                }
                let Ok(retry) = reset_consent(input, errors, "Retry this reset attempt? [y/N]")
                else {
                    writeln!(errors, "Reset failed. Could not read confirmation.")?;
                    return Ok(4);
                };
                if !retry {
                    return Ok(4);
                }
            }
        }
    }
}

/// Asks on `prompt_to`, which is stderr: stdout carries only outcome words.
fn reset_consent(
    input: &mut impl std::io::BufRead,
    prompt_to: &mut impl std::io::Write,
    prompt: &str,
) -> std::io::Result<bool> {
    writeln!(prompt_to, "{prompt}")?;
    prompt_to.flush()?;
    let mut answer = String::new();
    input.read_line(&mut answer)?;
    Ok(answer.trim().eq_ignore_ascii_case("y") || answer.trim().eq_ignore_ascii_case("yes"))
}
