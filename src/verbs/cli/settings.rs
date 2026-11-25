//! `tcr config show` and `tcr config set`: the saved settings beside the running ones, and saving one.

use std::path::Path;

use anyhow::Context as _;

use super::live_edit::{CONFIG_SET_WORDS, Live, LiveSite, fetch_running_settings, post_set_config};
use super::load_config;
use crate::base::config;
use crate::proxy::live_client::LiveControlError;

/// `tcr config show [--json]`: the rotation and limit settings this file
/// gives a server, defaults applied, through the same loader as
/// [`list_groups`]; and beside them, when a proxy answers on the file's port,
/// the settings it is running. See [`crate::base::config_show`].
pub async fn show_config(config_path: &Path, json: bool) -> anyhow::Result<()> {
    let document = config_show_document(config_path).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&document)?);
    } else {
        print!("{}", document.render_text());
    }
    Ok(())
}

/// The [`crate::base::config_show::ConfigShow`] `tcr config show` prints.
///
/// `running` is set only from a proxy that ANSWERED the route. Every other
/// outcome leaves it absent, and every outcome but "nothing is listening" says
/// why on stderr, because a proxy that is up but unreadable is not the same
/// fact as no proxy, and TcrBar reads stdout only.
pub async fn config_show_document(
    config_path: &Path,
) -> anyhow::Result<crate::base::config_show::ConfigShow> {
    let config = load_config(config_path)?;
    let saved = config.settings(config_path);
    let running = match fetch_running_settings(&config).await {
        Ok(running) => Some(running),
        Err(LiveControlError::NoServer) => None,
        Err(LiveControlError::NoRoute) => {
            eprintln!(
                "[tcr] the proxy on :{} is too old to report its settings (no {} route), so nothing is shown as waiting for a restart.",
                config.proxy.port,
                crate::proxy::CONFIG_PATH
            );
            None
        }
        Err(other) => {
            eprintln!(
                "[tcr] could not read the running proxy's settings on :{} ({}), so nothing is shown as waiting for a restart.",
                config.proxy.port,
                other.why()
            );
            None
        }
    };
    Ok(crate::base::config_show::ConfigShow { saved, running })
}

/// `tcr config set <key> <value>`: save one rotation or limit setting.
///
/// Parsed first ([`crate::base::config_set::Setting::parse`]): a refused value
/// writes nothing anywhere. Then the running proxy first, the same posture and
/// the same arms as [`set_control`], because the proxy owns the file while it
/// runs (see [`crate::proxy::CONFIG_PATH`]): it writes the key under its own
/// lock. With nothing listening the file is written here, through the same
/// [`crate::base::config_set::save_setting`]. A refused api-key or a refusal from
/// the route writes nothing; a proxy too old for the route, or one that
/// answered something unusable, gets the file written here with a warning.
///
/// Either way the running proxy keeps the value it booted with until it
/// restarts, so the line printed says so when one is running.
pub async fn set_config(config_path: &Path, key: &str, value: &str) -> anyhow::Result<()> {
    let setting =
        crate::base::config_set::Setting::parse(key, value).map_err(anyhow::Error::msg)?;
    let saved_line = |changed: bool| {
        if changed {
            format!("Saved {} = {}.", setting.key(), setting.value_words())
        } else {
            format!(
                "{} is already {}; nothing changed.",
                setting.key(),
                setting.value_words()
            )
        }
    };

    if let Ok(config) = config::load(config_path) {
        let site = LiveSite {
            config: &config,
            config_path,
            words: &CONFIG_SET_WORDS,
        };
        match site.decide(post_set_config(&config, &setting).await)? {
            Live::Applied(applied) => {
                println!(
                    "{} The proxy on :{} keeps its current value until it restarts.",
                    saved_line(applied.changed),
                    config.proxy.port
                );
                return Ok(());
            }
            Live::FileOnly(None) => {}
            Live::FileOnly(Some(stale)) => {
                let write = write_setting(config_path, &setting)?;
                eprintln!("{}", site.warning(&stale, None));
                println!(
                    "{}",
                    saved_line(write == crate::base::config_set::SettingWrite::Updated)
                );
                return Ok(());
            }
        }
    }

    let write = write_setting(config_path, &setting)?;
    println!(
        "{} The next proxy start uses it.",
        saved_line(write == crate::base::config_set::SettingWrite::Updated)
    );
    Ok(())
}

/// The file half of `tcr config set`.
fn write_setting(
    config_path: &Path,
    setting: &crate::base::config_set::Setting,
) -> anyhow::Result<crate::base::config_set::SettingWrite> {
    crate::base::config_set::save_setting(config_path, setting)
        .with_context(|| format!("save config at {}", config_path.display()))
}
