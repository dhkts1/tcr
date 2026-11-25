//! The routing document, schema 1: what a launcher reads from the running proxy, without
//! parsing text, to learn which model families each provider pool can serve now.
//!
//! Served at [`crate::proxy::ROUTING_PATH`] behind the gate every local route passes, and
//! printed by `tcr routing --json`. Its scope is a NEW, UNPINNED inference request that asks
//! for no group: a pinned conversation keeps its account whatever this says, and a `--group`
//! ask narrows the pool further.
//!
//! Whether a model is routable is the selector's own answer,
//! [`Manager::routing_candidates`], the candidate evaluation its picks run, never a second
//! copy of their gates. When none can serve, why and when one frees come from
//! that same evaluation at the stored hold, reset and revalidation deadlines.
//!
//! It names no account: no name, email, organization or account id, no session, no
//! credential and no upstream error text. `forecast` is `null` without observed history;
//! otherwise it carries demand estimates and cold calibration readiness.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::base::build_info::BuildInfo;
use crate::base::provider::Provider;
use crate::manager::{Manager, PoolPass};
use crate::records::observations::ErrorKind;

/// The schema this build writes, and the only one `tcr routing --json` accepts.
pub const SCHEMA: u32 = 1;

/// The words every unroutable model's reason starts with. A refusal of a model by every
/// account (`lastError` kind `capacity`) means exactly this, and never measured overload.
pub const NONE_CAN_SERVE: &str = "no available account can serve this model now";

/// The document. Its field names are the wire contract.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutingDocument {
    /// Always [`SCHEMA`] from this build.
    pub schema: u32,
    /// When the proxy built the document, RFC 3339 in UTC.
    pub generated_at: String,
    /// The serving binary's build.
    pub build: BuildInfo,
    /// `anthropic`, then `codex`.
    pub providers: Vec<ProviderRouting>,
    /// Demand estimates and calibration readiness, or `null` without observed history.
    pub forecast: Option<serde_json::Value>,
}

/// One provider pool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderRouting {
    pub provider: Provider,
    pub state: ProviderState,
    /// The last upstream error this boot, or `null` when none was seen.
    pub last_error: Option<LastError>,
    /// Every model family seen this boot.
    pub models: Vec<ModelRouting>,
    /// Checked total across all listed `codex` rows, parked included. Any unknown stays absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_credits_available: Option<u64>,
}

/// A pool's state for new work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderState {
    /// It can serve new work, no known model is blocked and no error came in ten minutes.
    Ok,
    /// It can serve new work, but a known model is blocked, no model has been seen yet, or an
    /// upstream error came within ten minutes.
    Degraded,
    /// No account can serve new work of this provider now.
    Out,
}

/// The last upstream error, and how often its kind came in the last ten minutes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LastError {
    pub kind: ErrorKind,
    /// When, RFC 3339 in UTC.
    pub at: String,
    /// Errors of `kind` on this provider in the ten minutes up to `generatedAt`.
    pub count10m: u32,
}

/// One model family.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelRouting {
    /// The family: `fable`, `opus`, `sonnet` or `haiku` for `anthropic`; the backend's own
    /// model id for `codex`.
    pub model: String,
    /// Every full id of this family seen this boot, sorted.
    pub ids: Vec<String>,
    /// Whether a new request for it would be served now.
    pub routable: bool,
    /// Why, in words.
    pub reason: String,
    /// When an account frees for it, RFC 3339 in UTC; `null` while it is routable, or when no
    /// account holding it out names a time.
    pub frees_at: Option<String>,
    /// How many accounts the pass that would serve it can choose from now.
    pub accounts_routable: u32,
}

/// The path a new inference request of `provider` arrives on, the one its selection is asked
/// about.
const fn inference_path(provider: Provider) -> &'static str {
    match provider {
        Provider::Anthropic => "/v1/messages",
        Provider::Codex => crate::base::provider::CODEX_RESPONSES_PATH,
    }
}

/// The document as of `now`, from what `manager` holds now. `None` only when a time cannot be
/// written as RFC 3339, which a clock inside years 0 to 9999 never produces.
pub fn document(manager: &Manager, now: OffsetDateTime) -> Option<RoutingDocument> {
    let providers = Provider::ALL
        .iter()
        .map(|&provider| provider_routing(manager, provider, now))
        .collect::<Option<Vec<_>>>()?;
    Some(RoutingDocument {
        schema: SCHEMA,
        generated_at: utc(now)?,
        build: BuildInfo::current(),
        providers,
        forecast: manager.forecast_document((now.unix_timestamp_nanos() / 1_000_000) as i64),
    })
}

fn provider_routing(
    manager: &Manager,
    provider: Provider,
    now: OffsetDateTime,
) -> Option<ProviderRouting> {
    let path = inference_path(provider);
    let now_ms = (now.unix_timestamp_nanos() / 1_000_000) as i64;
    let (last, ids) = manager.routing_observed(provider, now_ms);
    let models = catalogue(provider, &ids)
        .into_iter()
        .map(|(model, ids)| model_routing(manager, provider, path, model, ids, now))
        .collect::<Option<Vec<_>>>()?;
    let last_error = match last {
        Some((kind, at_ms, count10m)) => Some(LastError {
            kind,
            at: utc(
                OffsetDateTime::from_unix_timestamp_nanos(i128::from(at_ms) * 1_000_000).ok()?,
            )?,
            count10m,
        }),
        None => None,
    };
    let serving = manager.routing_candidates(now, None, path).pass.is_some();
    let state = state(serving, &models, last_error.as_ref());
    Some(ProviderRouting {
        provider,
        state,
        last_error,
        models,
        reset_credits_available: match provider {
            Provider::Anthropic => None,
            Provider::Codex => reset_credits_total(&manager.snapshot(now).accounts),
        },
    })
}

fn reset_credits_total(accounts: &[crate::records::stats::AccountSnapshot]) -> Option<u64> {
    accounts
        .iter()
        .filter(|account| account.provider == Provider::Codex)
        .try_fold(0u64, |total, account| {
            total.checked_add(account.codex.reset_credits.as_ref()?.available_count)
        })
}

/// The pool's state: out when nothing can serve its new work; degraded when something can
/// but a known model is blocked, no model has been seen yet, or an error came within ten
/// minutes; ok otherwise. An empty catalogue alone is never out.
fn state(serving: bool, models: &[ModelRouting], last_error: Option<&LastError>) -> ProviderState {
    if !serving {
        ProviderState::Out
    } else if models.is_empty()
        || models.iter().any(|model| !model.routable)
        || last_error.is_some_and(|error| error.count10m > 0)
    {
        ProviderState::Degraded
    } else {
        ProviderState::Ok
    }
}

/// The model families `provider` served, each with its full ids: an `anthropic` id under its
/// family, in [`crate::base::model::FAMILIES`] order, an id naming none inventing no family; a
/// `codex` id as its own model.
fn catalogue(provider: Provider, seen: &BTreeSet<String>) -> Vec<(String, Vec<String>)> {
    match provider {
        Provider::Anthropic => crate::base::model::FAMILIES
            .iter()
            .filter_map(|&family| {
                let ids: Vec<String> = seen
                    .iter()
                    .filter(|id| crate::base::model::family(id) == Some(family))
                    .cloned()
                    .collect();
                (!ids.is_empty()).then(|| (family.to_owned(), ids))
            })
            .collect(),
        Provider::Codex => seen
            .iter()
            .map(|id| (id.clone(), vec![id.clone()]))
            .collect(),
    }
}

fn model_routing(
    manager: &Manager,
    provider: Provider,
    path: &str,
    model: String,
    ids: Vec<String>,
    now: OffsetDateTime,
) -> Option<ModelRouting> {
    let found = manager.routing_candidates(now, ids.first().map(String::as_str), path);
    if let Some(pass) = found.pass {
        return Some(ModelRouting {
            model,
            ids,
            routable: true,
            reason: served_by(pass).to_owned(),
            frees_at: None,
            accounts_routable: found.accounts.len() as u32,
        });
    }
    let gates = match provider {
        Provider::Anthropic => crate::records::stats::describe_gates(&found.gated),
        Provider::Codex => crate::records::stats::describe_codex_gates(&found.gated),
    };
    let frees_at = match found.free_at {
        Some(at) => Some(utc(at)?),
        None => None,
    };
    Some(ModelRouting {
        model,
        ids,
        routable: false,
        reason: format!("{NONE_CAN_SERVE}{gates}"),
        frees_at,
        accounts_routable: 0,
    })
}

/// Which pass would serve a new request now, in words.
const fn served_by(pass: PoolPass) -> &'static str {
    match pass {
        PoolPass::Lock => "the account lock serves every request of this provider",
        PoolPass::Paced => "an account in rotation can serve it",
        PoolPass::LeastLoaded => "every account is paced; the least-loaded one serves it",
        PoolPass::FableLastResort => {
            "every account reads over its predicted Fable weekly limit; the freshest serves it as a last resort"
        }
        PoolPass::Revalidation => {
            "every account reads over its switch threshold; the least-utilized one serves it to revalidate"
        }
    }
}

/// `at` as RFC 3339 in UTC.
fn utc(at: OffsetDateTime) -> Option<String> {
    at.to_offset(time::UtcOffset::UTC).format(&Rfc3339).ok()
}

/// Read `body` as a schema-1 routing document. `Err` says why not: it is not JSON, it names
/// another schema or none, or it is a schema-1 document this build cannot read.
pub fn parse(body: &str) -> Result<RoutingDocument, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("the answer is not JSON ({e})"))?;
    match value.get("schema").and_then(serde_json::Value::as_u64) {
        Some(schema) if schema == u64::from(SCHEMA) => {}
        Some(schema) => {
            return Err(format!(
                "the answer is routing schema {schema}, and this build reads schema {SCHEMA}"
            ));
        }
        None => return Err("the answer names no routing schema".to_owned()),
    }
    serde_json::from_value(value)
        .map_err(|e| format!("the schema {SCHEMA} document is unreadable ({e})"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_total_includes_parked_omits_unknown_and_checks_overflow() {
        let manager_of = |rows: &[(&str, Option<u64>)]| {
            let accounts: Vec<_> = rows
                .iter()
                .enumerate()
                .map(|(index, (provider, _))| {
                    serde_json::json!({"name":format!("test-{index}"), "provider":provider,
                    "accessToken":"fake-key", "disabled":true})
                })
                .collect();
            let config = serde_json::from_value(serde_json::json!({"accounts":accounts})).unwrap();
            let manager = Manager::with_live_refresher(config, None);
            for (index, (_, count)) in rows.iter().enumerate() {
                if let Some(count) = count {
                    manager.apply_usage(
                        index,
                        &crate::records::codex_probe::usage_from_payload(&serde_json::json!({
                            "rate_limit_reset_credits":{"available_count":count}
                        })),
                    );
                }
            }
            manager
        };
        let now = OffsetDateTime::now_utc();
        let manager = manager_of(&[("codex", Some(2)), ("codex", Some(3)), ("anthropic", None)]);
        let doc = document(&manager, now).unwrap();
        assert_eq!(doc.schema, 1);
        assert_eq!(doc.providers[1].reset_credits_available, Some(5));
        let wire = serde_json::to_value(doc).unwrap();
        assert!(wire["providers"][0].get("resetCreditsAvailable").is_none());
        assert_eq!(wire["providers"][1]["resetCreditsAvailable"], 5);
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("fleet.json");
        let config: crate::base::config::Config =
            serde_json::from_value(serde_json::json!({"accounts":[
                {"name":"removed", "provider":"codex", "accessToken":"fake-key"},
                {"name":"retained", "provider":"codex", "accessToken":"fake-key", "disabled":true}
            ]}))
            .unwrap();
        std::fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
        let manager = Manager::with_live_refresher(config, Some(path.clone()));
        manager.apply_usage(
            1,
            &crate::records::codex_probe::usage_from_payload(&serde_json::json!({
                "rate_limit_reset_credits":{"available_count":0}
            })),
        );
        std::fs::write(&path, r#"{"accounts":[{"name":"retained","provider":"codex","accessToken":"fake-key","disabled":true}]}"#).unwrap();
        assert_eq!(
            document(&manager, now).unwrap().providers[1].reset_credits_available,
            Some(0)
        );
        for rows in [
            vec![("codex", None)],
            vec![
                ("codex", Some(i64::MAX as u64)),
                ("codex", Some(i64::MAX as u64)),
                ("codex", Some(2)),
            ],
        ] {
            let manager = manager_of(&rows);
            assert_eq!(
                document(&manager, now).unwrap().providers[1].reset_credits_available,
                None
            );
        }
        let manager = Manager::from_runtimes(vec![]);
        assert_eq!(
            document(&manager, now).unwrap().providers[1].reset_credits_available,
            Some(0)
        );
    }

    /// **The catalogue groups `anthropic` ids by family, in a fixed order, and invents no
    /// family; a `codex` id is its own model.**
    #[test]
    fn the_catalogue_groups_families_and_invents_none() {
        let seen: BTreeSet<String> = [
            "claude-opus-5-5",
            "claude-fable-5-1",
            "claude-opus-4-6",
            "claude-haiku-5-5",
            "some-other-model",
        ]
        .map(String::from)
        .into();
        let ids = |ids: &[&str]| ids.iter().map(|id| (*id).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            catalogue(Provider::Anthropic, &seen),
            vec![
                ("fable".to_owned(), ids(&["claude-fable-5-1"])),
                (
                    "opus".to_owned(),
                    ids(&["claude-opus-4-6", "claude-opus-5-5"])
                ),
                ("haiku".to_owned(), ids(&["claude-haiku-5-5"])),
            ]
        );
        let codex: BTreeSet<String> = ["gpt-b", "gpt-a"].map(String::from).into();
        assert_eq!(
            catalogue(Provider::Codex, &codex),
            vec![
                ("gpt-a".to_owned(), ids(&["gpt-a"])),
                ("gpt-b".to_owned(), ids(&["gpt-b"])),
            ]
        );
    }

    /// **Out only when nothing can serve; degraded for a blocked model, an empty catalogue
    /// or an error in ten minutes; ok otherwise.**
    #[test]
    fn the_state_rule() {
        let model = |routable| ModelRouting {
            model: "opus".to_owned(),
            ids: Vec::new(),
            routable,
            reason: String::new(),
            frees_at: None,
            accounts_routable: 0,
        };
        let error = |count10m| LastError {
            kind: ErrorKind::Quota,
            at: String::new(),
            count10m,
        };
        assert_eq!(state(false, &[model(true)], None), ProviderState::Out);
        assert_eq!(state(false, &[], None), ProviderState::Out);
        assert_eq!(
            state(true, &[], None),
            ProviderState::Degraded,
            "an empty catalogue alone is degraded, never out"
        );
        assert_eq!(
            state(true, &[model(true), model(false)], None),
            ProviderState::Degraded
        );
        assert_eq!(
            state(true, &[model(true)], Some(&error(1))),
            ProviderState::Degraded
        );
        assert_eq!(
            state(true, &[model(true)], Some(&error(0))),
            ProviderState::Ok,
            "an error older than ten minutes no longer degrades"
        );
        assert_eq!(state(true, &[model(true)], None), ProviderState::Ok);
    }

    /// **The reader takes schema 1, a forecast object included, and refuses every other
    /// answer with a reason.**
    #[test]
    fn parse_takes_schema_one_and_refuses_the_rest() {
        let document = r#"{"schema":1,"generatedAt":"2026-10-26T00:00:00Z",
            "build":{"sha":"abc","dirty":false,"builtAt":"2026-10-26T00:00:00Z"},
            "providers":[],"forecast":null}"#;
        assert_eq!(parse(document).map(|d| d.schema), Ok(1));
        let later = document.replace(r#""forecast":null"#, r#""forecast":{"providers":[]}"#);
        assert!(
            parse(&later).is_ok(),
            "a later schema-1 forecast object is read"
        );
        let refused = |body: &str| parse(body).err().unwrap_or_default();
        assert!(refused("not json").contains("not JSON"));
        assert!(refused(r#"{"schema":2}"#).contains("schema 2"));
        assert!(refused("{}").contains("names no routing schema"));
        assert!(refused(r#"{"schema":1}"#).contains("unreadable"));
    }
}
