//! "Running proxy first, file second": the decision every editing verb applies to a live
//! answer, the words each verb's warnings use, and the routes those verbs post to.

use std::path::Path;

use anyhow::bail;

use super::live_client::{ControlRoute, LiveControlError, RESTART_HINT, call_route};
use crate::config::Config;

/// What a verb that asks the running proxy first may do with its file.
pub(super) enum Live<T> {
    /// The running proxy applied it, and wrote the file if it owns that half.
    Applied(T),
    /// Write the file here. `None` when nothing is listening, the ordinary
    /// offline case, which says nothing. `Some` when a proxy is there and the
    /// change did not reach it, which is never silent: see [`LiveSite::warning`].
    FileOnly(Option<Stale>),
}

/// Why a running proxy did not take a change the file is about to get.
pub(super) enum Stale {
    /// An older tcr with no route for it.
    NoRoute,
    /// It answered something unusable, or nothing in time; the reason.
    Failed(String),
}

/// The words one verb's refusals and warnings use for its own consequence.
/// The policy around them is [`LiveSite::decide`], one copy for every verb.
pub(super) struct LiveWords {
    /// The route the verb calls, named in the "too old" warning.
    pub(super) route_path: &'static str,
    /// Follows "rejected the api-key in <path>": why nothing was written.
    unauthorized: &'static str,
    /// Follows the server's own refusal message.
    rejected: &'static str,
    /// Follows "is too old to".
    too_old_to: &'static str,
    /// Follows "could not".
    could_not: &'static str,
    /// Follows "so": what happened to the file.
    file_written: &'static str,
}

/// After a refusal, on the verbs whose file half changes one account row.
const NOTHING_CHANGED: &str = " Nothing was changed.";
/// After a fallback write, on the same verbs.
const ONLY_THE_FILE: &str = "only the config file was changed";

/// `tcr rm`. Deleting the record beside a proxy that refused our key would
/// leave it rotating an account that no longer exists on disk, with no record
/// left to reconcile against on restart.
pub(super) const REMOVE_WORDS: LiveWords = LiveWords {
    route_path: crate::proxy::DISABLED_PATH,
    unauthorized: " — the config was NOT changed, because deleting it would leave the running proxy still rotating this account.",
    rejected: NOTHING_CHANGED,
    too_old_to: "accept live account control",
    could_not: "apply this to",
    file_written: ONLY_THE_FILE,
};

/// `tcr priority`.
pub(super) const PRIORITY_WORDS: LiveWords = LiveWords {
    route_path: crate::proxy::PRIORITY_PATH,
    unauthorized: ", so the config was NOT changed, because writing it would leave the running proxy on the old order.",
    rejected: NOTHING_CHANGED,
    too_old_to: "accept a live priority change",
    could_not: "apply this to",
    file_written: ONLY_THE_FILE,
};

/// `tcr config set`. The proxy owns the file while it runs, so a fallback
/// write is this process writing beside it, and a refusal carries the
/// server's message alone.
pub(super) const CONFIG_SET_WORDS: LiveWords = LiveWords {
    route_path: crate::proxy::CONFIG_PATH,
    unauthorized: ", so nothing was changed: writing the file beside a running proxy can lose the change to its own next write.",
    rejected: "",
    too_old_to: "save settings itself",
    could_not: "hand this to",
    file_written: "this process wrote the file beside it",
};

/// `tcr enable` and `tcr disable`. Writing the file beside a proxy that
/// refused our key would be the config saying benched while the proxy keeps
/// rotating.
pub(super) const ENABLE_WORDS: LiveWords = LiveWords {
    route_path: crate::proxy::DISABLED_PATH,
    unauthorized: " — the config was NOT changed, because writing it would leave the running proxy still rotating this account.",
    rejected: NOTHING_CHANGED,
    too_old_to: "accept live account control",
    could_not: "apply this to",
    file_written: ONLY_THE_FILE,
};

/// `tcr control`. The running proxy is the one that resolves identity
/// traffic, so a file naming a new control account beside it is a lie.
pub(super) const CONTROL_WORDS: LiveWords = LiveWords {
    route_path: crate::proxy::CONTROL_PATH,
    unauthorized: " — the config was NOT changed, because writing it would leave the running proxy still resolving identity traffic against its old control account.",
    rejected: NOTHING_CHANGED,
    too_old_to: "accept live account control",
    could_not: "apply this to",
    file_written: ONLY_THE_FILE,
};

/// One verb's "running proxy first, file second" decision, at one config.
pub(super) struct LiveSite<'a> {
    pub(super) config: &'a Config,
    pub(super) config_path: &'a Path,
    pub(super) words: &'static LiveWords,
}

impl LiveSite<'_> {
    /// The decision table. Nothing listening writes the file quietly. A
    /// rejected api-key or a refused query changes nothing: the file's own
    /// resolution could land on a different row than the server matched, and a
    /// file written beside a proxy we could not talk to says one thing while the
    /// proxy does another. A missing route or an unusable answer writes the file
    /// and warns.
    pub(super) fn decide<T>(&self, answer: Result<T, LiveControlError>) -> anyhow::Result<Live<T>> {
        let port = self.config.proxy.port;
        match answer {
            Ok(applied) => Ok(Live::Applied(applied)),
            Err(LiveControlError::NoServer) => Ok(Live::FileOnly(None)),
            Err(LiveControlError::Unauthorized) => bail!(
                "the proxy on :{port} rejected the api-key in {}{} Fix `proxy.apiKey` and retry.",
                self.config_path.display(),
                self.words.unauthorized,
            ),
            Err(LiveControlError::Rejected(message)) => bail!(
                "the proxy running on :{port} refused this: {message}{}",
                self.words.rejected
            ),
            Err(LiveControlError::NoRoute) => Ok(Live::FileOnly(Some(Stale::NoRoute))),
            Err(other) => Ok(Live::FileOnly(Some(Stale::Failed(other.why())))),
        }
    }

    /// The warning for a file written beside a proxy that did not take the
    /// change. `keeps` names what the running proxy goes on using, as in "It
    /// will KEEP <keeps> until it restarts"; `None` leaves that sentence out.
    pub(super) fn warning(&self, stale: &Stale, keeps: Option<&str>) -> String {
        let port = self.config.proxy.port;
        let words = self.words;
        match stale {
            Stale::NoRoute => format!(
                "[tcr] WARNING: the proxy running on :{port} is too old to {} (no {} route), so {}.{} {RESTART_HINT}",
                words.too_old_to,
                words.route_path,
                words.file_written,
                keeps.map_or_default(|k| format!(" It will KEEP {k} until it restarts.")),
            ),
            Stale::Failed(why) => format!(
                "[tcr] WARNING: could not {} the proxy running on :{port} ({why}), so {}.{}",
                words.could_not,
                words.file_written,
                keeps.map_or_default(|k| format!(" It may KEEP {k} until it restarts.")),
            ),
        }
    }
}

const DISABLED_ROUTE: ControlRoute = ControlRoute {
    path: crate::proxy::DISABLED_PATH,
    endpoint: crate::proxy::DISABLED_ENDPOINT,
    refusals: &[404, 409],
    noun: "account-control",
    success_needs_stamp: false,
};

const CONTROL_ROUTE: ControlRoute = ControlRoute {
    path: crate::proxy::CONTROL_PATH,
    endpoint: crate::proxy::CONTROL_ENDPOINT,
    refusals: &[404, 409],
    noun: "control-account",
    success_needs_stamp: false,
};

const PRIORITY_ROUTE: ControlRoute = ControlRoute {
    path: crate::proxy::PRIORITY_PATH,
    endpoint: crate::proxy::PRIORITY_ENDPOINT,
    refusals: &[404, 409],
    noun: "priority",
    success_needs_stamp: false,
};

/// Both halves of [`crate::proxy::CONFIG_PATH`], the `GET` and the `POST`.
const CONFIG_ROUTE: ControlRoute = ControlRoute {
    path: crate::proxy::CONFIG_PATH,
    endpoint: crate::proxy::CONFIG_ENDPOINT,
    refusals: &[400, 409],
    noun: "settings",
    success_needs_stamp: true,
};

/// Ask the running proxy to park/unpark an account.
pub(super) async fn post_set_disabled(
    config: &Config,
    query: &str,
    disabled: bool,
) -> Result<crate::proxy::SetDisabledResponse, LiveControlError> {
    let body = serde_json::json!({ "query": query, "disabled": disabled });
    call_route(config, &DISABLED_ROUTE, |client, url| {
        client.post(url).json(&body)
    })
    .await
    .result
}

/// Ask the running proxy to set or clear the identity-bound control account.
///
/// `query = None` is the CLEAR request; it is sent through exactly like a
/// name, since [`crate::proxy::SetControlRequest`] treats `query: null` as a
/// complete operation on its own, not an error.
pub(super) async fn post_set_control(
    config: &Config,
    query: Option<&str>,
) -> Result<crate::proxy::SetControlResponse, LiveControlError> {
    let body = serde_json::json!({ "query": query });
    call_route(config, &CONTROL_ROUTE, |client, url| {
        client.post(url).json(&body)
    })
    .await
    .result
}

/// Ask the running proxy to set one account's priority in its live rotation.
pub(super) async fn post_set_priority(
    config: &Config,
    query: &str,
    priority: i64,
) -> Result<crate::proxy::SetPriorityResponse, LiveControlError> {
    let body = serde_json::json!({ "query": query, "priority": priority });
    call_route(config, &PRIORITY_ROUTE, |client, url| {
        client.post(url).json(&body)
    })
    .await
    .result
}

/// Hand one setting to the running proxy, which writes it to its config file.
pub(super) async fn post_set_config(
    config: &Config,
    setting: &crate::config_set::Setting,
) -> Result<crate::proxy::SetConfigResponse, LiveControlError> {
    let body = serde_json::json!({
        "key": setting.key().as_str(),
        "value": setting.value_text(),
    });
    call_route(config, &CONFIG_ROUTE, |client, url| {
        client.post(url).json(&body)
    })
    .await
    .result
}

/// The settings the running proxy is using: its `GET` on
/// [`crate::proxy::CONFIG_PATH`].
pub(super) async fn fetch_running_settings(
    config: &Config,
) -> Result<crate::config_show::ConfigSettings, LiveControlError> {
    call_route(config, &CONFIG_ROUTE, |client, url| client.get(url))
        .await
        .result
}
