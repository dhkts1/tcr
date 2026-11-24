//! The verbs that change one account: `rm`, `priority`, `enable`/`disable`, `control` and its
//! exit lock, plus `request-key` and printing an account's access token.

use std::path::Path;

use anyhow::{Context as _, bail};

use super::account_query::resolve_account;
use super::live_client::{RESTART_HINT, fetch_live_status};
use super::live_edit::{
    CONTROL_WORDS, ENABLE_WORDS, Live, LiveSite, PRIORITY_WORDS, REMOVE_WORDS, post_set_control,
    post_set_disabled, post_set_priority,
};
use super::{edit_account, load_config, load_for_edit};
use crate::config::{self, Config};
use crate::provider::Provider;

/// How to set an account's priority: an explicit integer, or a relative
/// `--first` / `--last` that recomputes against the existing fleet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriorityArg {
    /// Set the exact integer priority.
    N(i64),
    /// Make this account sort FIRST: `min(0, existing priorities) - 1`.
    First,
    /// Make this account sort LAST: `max(0, existing priorities) + 1`.
    Last,
}

/// Remove the account matching `query` from the config and save — live-first,
/// same discipline as [`set_enabled`].
///
/// Resolution happens BEFORE any mutation, so a non-matching query returns an
/// error with the file left byte-identical (no partial write). Order is now
/// resolve -> live-disable -> delete-from-config: deleting first and disabling
/// second would leave a window (or, on a failed live call, a permanent state)
/// where the config says the account does not exist while the running proxy
/// keeps rotating it — the exact lie this change exists to eliminate, except
/// now the record is gone too and unrecoverable.
///
/// `Unauthorized` and `Rejected` bail with the config untouched, for the same
/// reasons as `set_enabled` — see its doc-comment — sharpened: a removal
/// applied on top of a live disagreement cannot be undone by re-adding, since
/// the account's credentials are gone from the file being edited.
///
/// The file half is [`config::remove_account_entry`], a raw-document delete,
/// so a quarantined row elsewhere in the file no longer blocks it. The parked
/// row then LEAVES the running proxy the next time that proxy reads the file
/// (`Manager::reload_groups_if_changed`, which every status request runs
/// first), so the panel's next poll no longer lists it.
pub async fn remove_account(config_path: &Path, query: &str) -> anyhow::Result<()> {
    // Everything the FILE could refuse is checked before the running proxy is
    // touched. The live step parks the account, and parking is a side effect:
    // run first, a delete the file then refused (it used to refuse any file
    // holding one quarantined row) left the account parked and still listed,
    // which from the panel reads as "delete did nothing" and, tried again on a
    // now-parked row, as "delete does not work on parked accounts".
    let config = load_config(config_path)?;
    if let Some(why) = &config.rename_write_error {
        bail!(
            "config at {} holds accounts sharing a name, and the rename that would fix it could \
             not be written back ({why}) — so nothing was changed. Fix the file's permissions, or \
             give each row its own name by hand, and retry.",
            config_path.display()
        );
    }
    let idx = resolve_account(&config.accounts, query)?;
    let target = config.accounts[idx].clone();
    let name = target.name.clone();

    let site = LiveSite {
        config: &config,
        config_path,
        words: &REMOVE_WORDS,
    };
    let (parked_live, warning) = match site.decide(post_set_disabled(&config, &name, true).await)? {
        Live::Applied(_) => (true, None),
        Live::FileOnly(stale) => (
            false,
            stale.map(|stale| site.warning(&stale, Some(&format!("routing to '{name}'")))),
        ),
    };

    let failed = match config::remove_account_entry(config_path, &target)
        .with_context(|| format!("save config at {}", config_path.display()))
    {
        Ok(config::AccountRemoval::Removed { .. }) => None,
        Ok(config::AccountRemoval::NoEntry) => Some(anyhow::anyhow!(
            "'{name}' vanished from {} between load and write (concurrent edit?), so nothing was removed",
            config_path.display()
        )),
        Ok(config::AccountRemoval::Ambiguous) => Some(anyhow::anyhow!(
            "more than one entry in {} carries '{name}''s identity, so nothing was removed",
            config_path.display()
        )),
        Err(err) => Some(err),
    };
    if let Some(err) = failed {
        // The proxy parked it a moment ago and the delete did not happen: put
        // it back the way it was, so a failed delete changes nothing.
        if parked_live
            && target.disabled != Some(true)
            && let Err(undo) = post_set_disabled(&config, &name, false).await
        {
            return Err(err.context(format!(
                    "the delete failed after the proxy on :{} had parked '{name}', and un-parking it failed too ({}), so it stays parked",
                    config.proxy.port,
                    undo.why()
                )));
        }
        return Err(err);
    }
    if let Some(warning) = warning {
        eprintln!("{warning}");
    }
    // The running proxy drops a row the file no longer holds the next time it
    // reads the file, which a status request does first. Ask it once, so a
    // proxy built before it could do that is NAMED here, on the stderr TcrBar
    // shows on the row, rather than leaving the row listed with no reason.
    if parked_live
        && let Ok(payload) = fetch_live_status(&config).await
        && payload.lists_account(&name)
    {
        eprintln!(
            "[tcr] the proxy running on :{} stopped routing to '{name}', but it predates live removal and keeps listing it as disabled until it restarts. {}",
            config.proxy.port, RESTART_HINT,
        );
    }
    println!("Removed account '{name}'.");
    Ok(())
}

/// The integer a [`PriorityArg`] means against `config`'s accounts of `provider`.
///
/// `--first` = `min(0, existing priorities) - 1`; `--last` =
/// `max(0, existing priorities) + 1`; an explicit `N` is verbatim. The `0` seed
/// guarantees a relative move always crosses the default tier even when every
/// existing priority sits on the same side of it.
///
/// "Existing" means the priorities of `provider`'s accounts, the row's own: Claude
/// and Codex accounts rotate apart, since neither serves the other's requests,
/// so a Codex account's priority says nothing about where a Claude account sits.
/// In a fleet with no Codex account that is every account, as it always was.
pub(super) fn priority_value(config: &Config, provider: Provider, priority: PriorityArg) -> i64 {
    let existing = config
        .accounts
        .iter()
        .filter(|a| a.provider == provider)
        .filter_map(|a| a.priority)
        .chain(std::iter::once(0));
    match priority {
        PriorityArg::N(n) => n,
        PriorityArg::First => existing.min().unwrap_or(0) - 1,
        PriorityArg::Last => existing.max().unwrap_or(0) + 1,
    }
}

/// What `tcr priority` prints once the value is set: `Set priority of '{name}'
/// to {value}.`, except that a Codex account moved with `--first` or `--last`
/// says which end of the Codex rotation it now holds, since that is the only
/// rotation the move was taken against.
pub(super) fn priority_message(
    name: &str,
    value: i64,
    provider: Provider,
    arg: PriorityArg,
) -> String {
    let plain = || format!("Set priority of '{name}' to {value}.");
    match provider {
        Provider::Anthropic => plain(),
        Provider::Codex => match arg {
            PriorityArg::First => crate::provider_guard::codex_priority_line(name, value, true),
            PriorityArg::Last => crate::provider_guard::codex_priority_line(name, value, false),
            PriorityArg::N(_) => plain(),
        },
    }
}

/// Set the priority of the account matching `query`, **in the running proxy
/// first** and then in the config file, the same posture as [`set_enabled`].
///
/// A file-only write was the gap: the proxy reads `priority` once at boot, so
/// `tcr priority alice --first` changed nothing until a restart, and a restart
/// costs every live session its warm prompt cache.
///
/// The value is computed from the FILE (so `--first`/`--last` mean what they
/// always meant), the query is resolved in the file before anything is sent,
/// and the server ([`crate::proxy::PRIORITY_PATH`]) applies that integer in
/// memory only. This function writes the file. The arms follow
/// [`set_enabled`]: a refused api-key or a refused query changes nothing; a
/// missing route or an unusable answer writes the file and says the running
/// proxy keeps the old order until it restarts.
pub async fn set_priority(
    config_path: &Path,
    query: &str,
    priority: PriorityArg,
) -> anyhow::Result<()> {
    let config = load_for_edit(config_path)?;
    let idx = resolve_account(&config.accounts, query)?;
    let provider = config.accounts[idx].provider;
    let value = priority_value(&config, provider, priority);

    let site = LiveSite {
        config: &config,
        config_path,
        words: &PRIORITY_WORDS,
    };
    match site.decide(post_set_priority(&config, query, value).await)? {
        Live::Applied(applied) => {
            write_priority(config_path, query, value).with_context(|| {
                format!(
                    "the running proxy already ranks '{}' at {value}, but the config file was not updated, so a restart will undo it",
                    applied.name
                )
            })?;
            println!(
                "{}",
                priority_message(&applied.name, value, provider, priority)
            );
        }
        Live::FileOnly(stale) => {
            let name = write_priority(config_path, query, value)?;
            if let Some(stale) = stale {
                eprintln!("{}", site.warning(&stale, Some("the old order")));
            }
            println!("{}", priority_message(&name, value, provider, priority));
        }
    }
    Ok(())
}

/// The file half of `tcr priority`: resolve, set, save, return the resolved
/// name.
fn write_priority(config_path: &Path, query: &str, value: i64) -> anyhow::Result<String> {
    edit_account(config_path, query, |config, idx| {
        config.accounts[idx].priority = Some(value);
        config.accounts[idx].name.clone()
    })
}

/// `tcr token <query>` — print the matched account's access token to stdout,
/// nothing else, so a caller can pipe or copy it. Exists for TcrBar's "Copy
/// Access Token" row action: the app never reads the config file itself
/// (it holds live credentials), so every credential read is a subprocess.
///
/// Reads the FILE, not the running proxy — close enough, because every
/// refresh the proxy performs is persisted straight back to the file
/// (`Manager::persist_tokens`); the window where the two differ is the
/// write itself. Read-only: a non-matching query errors with the file
/// untouched, and nothing is logged — the token goes to stdout only.
/// The request key in the config at `config_path`, for `tcr request-key`. Read
/// only: generating one is the server's job at boot, so a launcher and the
/// server can never hold two different keys. With none, an error that says so.
pub fn request_key(config_path: &Path) -> anyhow::Result<String> {
    let key = config::read_request_key(config_path)
        .with_context(|| format!("could not read {}", config_path.display()))?;
    key.ok_or_else(|| {
        anyhow::anyhow!(
            "no request key in {}: the tcr server writes one there when it starts",
            config_path.display()
        )
    })
}

pub fn print_access_token(config_path: &Path, query: &str) -> anyhow::Result<()> {
    let config = load_config(config_path)?;
    let idx = resolve_account(&config.accounts, query)?;
    println!("{}", config.accounts[idx].access_token);
    Ok(())
}

/// Enable or disable the account matching `query`, **in the running proxy first**.
///
/// A file-only write was the bug. The proxy reads `disabled` from the config once,
/// at `Manager::new`, and never again — so `tcr disable alice` exited 0, printed
/// "Disabled account 'alice'.", and the serving process kept handing that account
/// live traffic. `tcr status` prefers the live server, so nothing anywhere said
/// otherwise. Two accounts were measured in that state on the live fleet.
///
/// So: ask the server ([`crate::proxy::DISABLED_PATH`]), which applies the flag to
/// its in-memory rotation AND persists it, and fall back to the file only when no
/// server answered. The fallback is not silent when a server DID answer and could
/// not do the job — that silence is the entire reason the defect was invisible.
///
/// `disabled = false` on the file path sets the field to `None` so
/// `skip_serializing_if = Option::is_none` DROPS the key entirely rather than
/// writing a `false` literal. `Manager::set_disabled`'s
/// persist does the same, so both paths leave the same document.
pub async fn set_enabled(config_path: &Path, query: &str, disabled: bool) -> anyhow::Result<()> {
    // A config we cannot read has no port and no api-key to reach a server with;
    // fall through to the file path, which reports the load failure as it always
    // has. Never a silent skip of the live attempt for any other reason.
    if let Ok(config) = config::load(config_path) {
        // A missing route is HALF a disable, the arm that used to be the whole
        // function, silently; the warning says so.
        let site = LiveSite {
            config: &config,
            config_path,
            words: &ENABLE_WORDS,
        };
        match site.decide(post_set_disabled(&config, query, disabled).await)? {
            Live::Applied(applied) => {
                println!(
                    "{} account '{}'.",
                    if disabled { "Disabled" } else { "Enabled" },
                    applied.name
                );
                if let Some(warning) = &applied.warning {
                    eprintln!("[tcr] warning: {warning}");
                }
                return Ok(());
            }
            Live::FileOnly(None) => {}
            Live::FileOnly(Some(stale)) => {
                let name = write_disabled_flag(config_path, query, disabled)?;
                let keeps = format!(
                    "{} '{name}'",
                    if disabled { "routing to" } else { "benching" }
                );
                eprintln!("{}", site.warning(&stale, Some(&keeps)));
                return Ok(());
            }
        }
    }

    let name = write_disabled_flag(config_path, query, disabled)?;
    println!(
        "{} account '{name}'.",
        if disabled { "Disabled" } else { "Enabled" }
    );
    Ok(())
}

/// The file half of enable/disable: resolve, set the flag, save, return the
/// resolved name. Exactly what `set_enabled` did before it learned to ask the
/// server.
fn write_disabled_flag(config_path: &Path, query: &str, disabled: bool) -> anyhow::Result<String> {
    edit_account(config_path, query, |config, idx| {
        config.accounts[idx].disabled = if disabled { Some(true) } else { None };
        config.accounts[idx].name.clone()
    })
}

/// The file half of `tcr control`: resolve OFFLINE (when `query` is `Some`) →
/// persist ONLY the top-level `controlAccount` key via
/// [`config::save_control_account`] — mirroring [`write_disabled_flag`]'s
/// shape, but never a whole-config [`config::save`]; see
/// `save_control_account`'s doc-comment and `1d978ce` for why that clobbers
/// out-of-band keys.
pub(super) fn write_control_account(
    config_path: &Path,
    query: Option<&str>,
) -> anyhow::Result<Option<String>> {
    let config = load_for_edit(config_path)?;
    let target = match query {
        None => None,
        Some(q) => Some(config.accounts[resolve_account(&config.accounts, q)?].clone()),
    };
    // The live route's refusal, offline: boot ignores a `controlAccount` naming a Codex
    // account, so writing one would persist a setting the next start throws away.
    if let Some(account) = &target {
        match account.provider {
            Provider::Anthropic => {}
            Provider::Codex => bail!(
                "{} Nothing was changed.",
                crate::manager::codex_control_refusal(&account.name)
            ),
        }
    }
    let name = target.as_ref().map(|a| a.name.clone());
    let outcome = config::save_control_account(config_path, target.as_ref())
        .with_context(|| format!("save config at {}", config_path.display()))?;
    if outcome == config::ControlWrite::Ambiguous {
        bail!(
            "more than one config entry shares this account's identity — nothing was changed. \
             Give each its own accountUuid/orgUuid and retry."
        );
    }
    Ok(name)
}

/// Write one account's exit lock: where its requests leave from, and what an
/// exit it cannot reach costs.
///
/// Through [`edit_account`], the same load/resolve/save chain every other
/// account edit uses, so this verb cannot resolve a query differently from
/// `tcr disable`, cannot skip the ambiguous-query message, and cannot leave a
/// partial write behind when the query matches nothing.
///
/// # File only, and that is not the half-write `set_enabled` shouts about
///
/// `egress` is a boot-time field: the request path reads the pin off the
/// config the proxy loaded at start, so a running proxy keeps its old exit
/// until it restarts, and there is no live route to ask instead. That is a
/// different situation from a disable, where the file and a running rotation
/// can disagree about an account that is being used RIGHT NOW. The caller
/// prints the restart line rather than this returning a warning, because the
/// panel shells out to the CLI and the operator reads what it prints.
///
/// `None` leaves that half alone: an operator who says `--must` and nothing
/// else is changing the strictness of a pin they already chose, and re-sending
/// a pin they did not name would be this function inventing one.
///
/// # A Codex account always leaves from this Mac
///
/// No Mac carries a Codex request in this version, so a pin to another Mac is
/// one the exit path can never honour: a loose one is ignored, and a strict one
/// makes the account ineligible for selection. Both halves that would make
/// such a pin, `--exits-from <peer>` and `--must`, are therefore refused on a
/// Codex row before anything is written ([`provider_guard`]). `--exits-from
/// local` and `--no-must` stay allowed, so a pin written before this refusal
/// existed can still be cleared.
///
/// The check reads the file with [`load_config`], never [`load_for_edit`],
/// whose running-proxy warning [`edit_account`] already prints. A query the
/// check cannot resolve, or a file it cannot read, is left to [`edit_account`],
/// so those errors read exactly as they always have.
///
/// [`provider_guard`]: crate::provider_guard::provider_guard
pub fn set_account_egress(
    config_path: &Path,
    query: &str,
    egress: Option<config::Egress>,
    strict: Option<bool>,
) -> anyhow::Result<(String, config::EgressPin)> {
    let pins_away = matches!(egress, Some(config::Egress::Via(_))) || strict == Some(true);
    if pins_away
        && let Ok(config) = load_config(config_path)
        && let Ok(idx) = resolve_account(&config.accounts, query)
    {
        crate::provider_guard::provider_guard(
            &config.accounts[idx],
            crate::provider_guard::ClaudeOnlyVerb::ExitFromPeer,
        )
        .map_err(anyhow::Error::msg)?;
    }
    edit_account(config_path, query, |config, idx| {
        if let Some(egress) = egress {
            config.accounts[idx].egress = egress;
        }
        if let Some(strict) = strict {
            config.accounts[idx].egress_strict = strict;
        }
        (
            config.accounts[idx].name.clone(),
            config.accounts[idx].egress_pin(),
        )
    })
}

/// Set or clear the identity-bound control account, **in the running proxy
/// first** — same posture as [`set_enabled`], and for the same reason: a
/// file-only write would leave the running process still resolving identity
/// traffic to its OLD control account (or none) until it restarts.
pub async fn set_control(config_path: &Path, query: Option<&str>) -> anyhow::Result<()> {
    // A config we cannot read has no port and no api-key to reach a server
    // with; fall through to the file path, which reports the load failure as
    // it always has. Never a silent skip of the live attempt for any other
    // reason.
    if let Ok(config) = config::load(config_path) {
        let site = LiveSite {
            config: &config,
            config_path,
            words: &CONTROL_WORDS,
        };
        match site.decide(post_set_control(&config, query).await)? {
            Live::Applied(applied) => {
                match &applied.name {
                    Some(name) => println!("Set control account to '{name}'."),
                    None => println!("Cleared the control account."),
                }
                if let Some(warning) = &applied.warning {
                    eprintln!("[tcr] warning: {warning}");
                }
                return Ok(());
            }
            Live::FileOnly(None) => {}
            Live::FileOnly(Some(stale)) => {
                write_control_account(config_path, query)?;
                eprintln!("{}", site.warning(&stale, Some("its OLD control account")));
                return Ok(());
            }
        }
    }

    let name = write_control_account(config_path, query)?;
    match name {
        Some(name) => println!("Set control account to '{name}'."),
        None => println!("Cleared the control account."),
    }
    Ok(())
}

/// `tcr control --show` — print the current control account, preferring the
/// LIVE server's answer (it may differ from the file if the server has not
/// been restarted since a config edit) and falling back to the file.
pub async fn show_control(config_path: &Path) -> anyhow::Result<()> {
    let config = load_config(config_path)?;
    let control = match fetch_live_status(&config).await {
        Ok(payload) => payload.control,
        Err(_) => config
            .control_account
            .as_ref()
            .map(|c| c.name().to_string()),
    };
    match control {
        Some(name) => println!("{name}"),
        None => println!("(none)"),
    }
    Ok(())
}
