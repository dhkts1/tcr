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
//! [`Manager::exhaustion_hint`], the view the synthetic 429 reports.
//!
//! It names no account: no name, email, organization or account id, no session, no
//! credential and no upstream error text. `forecast` is `null` until the forecast rounds fill
//! it, and a reader of schema 1 takes any JSON value there.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::build_info::BuildInfo;
use crate::manager::{Manager, PoolPass};
use crate::provider::Provider;

/// The schema this build writes, and the only one `tcr routing --json` accepts.
pub const SCHEMA: u32 = 1;

/// The words every unroutable model's reason starts with. A refusal of a model by every
/// account (`lastError` kind `capacity`) means exactly this, and never measured overload.
pub const NONE_CAN_SERVE: &str = "no available account can serve this model now";

/// `lastError.count10m`'s window, in seconds: ten minutes.
const ERROR_WINDOW_SECS: i64 = 600;

/// Most full model ids remembered per provider. They come from served responses, so a real
/// fleet sees a handful; the cap only bounds memory against an upstream that echoes anything.
const MODEL_IDS_CAP: usize = 64;

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
    /// `null` until the forecast rounds fill it.
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

/// What an upstream error was, by where tcr observed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ErrorKind {
    /// The upstream said it is overloaded: a 529 (each step of `proxy`'s 529 ladder), a 503,
    /// or a stream that failed in-band as overloaded.
    Overloaded,
    /// No available account can serve the requested model now: a `codex` account refused the
    /// model for its plan. Not measured overload, and not proof the model is gone: an account
    /// that supports it may simply be unavailable right now.
    Capacity,
    /// A 429: a quota rejection or a rate limit.
    Quota,
    /// The credential was refused: a 401, or a 403 that holds the account out
    /// ([`Manager::mark_refused`]).
    Auth,
    /// No usable answer: the send failed, the body could not be read, or a stream proved
    /// truncated.
    Transport,
}

impl ErrorKind {
    const ALL: [Self; 5] = [
        Self::Overloaded,
        Self::Capacity,
        Self::Quota,
        Self::Auth,
        Self::Transport,
    ];

    /// The kind an upstream answer's status alone names, `None` for a status that names none.
    /// A 403 is not here: only the one that refuses the credential is auth, and
    /// [`Manager::mark_refused`] records that one.
    pub const fn of_status(status: u16) -> Option<Self> {
        match status {
            401 => Some(Self::Auth),
            429 => Some(Self::Quota),
            503 | 529 => Some(Self::Overloaded),
            _ => None,
        }
    }

    /// The kind a failed stream's recorded kind ([`Manager::record_stream_error`]) names: a
    /// proved truncation is transport, an in-band overloaded error is overloaded, and any other
    /// in-band error names none.
    pub fn of_stream(kind: &str) -> Option<Self> {
        match kind {
            crate::proxy::TRUNCATED_STREAM_ERROR_KIND => Some(Self::Transport),
            "overloaded_error" | "server_is_overloaded" => Some(Self::Overloaded),
            _ => None,
        }
    }

    const fn index(self) -> usize {
        self as usize
    }
}

/// One provider's upstream errors: the last, and how many of each kind fell in each second
/// of the last ten minutes. Fixed memory, one slot per second of the window reused as the
/// window moves, so no burst can grow it.
#[derive(Debug, Clone)]
struct ErrorLog {
    last: Option<(ErrorKind, i64)>,
    /// `(second, counts by kind)` at slot `second % ERROR_WINDOW_SECS`. A slot holding an
    /// older second is stale, and the next write to it starts it over.
    slots: Vec<(i64, [u32; ErrorKind::ALL.len()])>,
}

impl Default for ErrorLog {
    fn default() -> Self {
        Self {
            last: None,
            slots: vec![(i64::MIN, [0; ErrorKind::ALL.len()]); ERROR_WINDOW_SECS as usize],
        }
    }
}

impl ErrorLog {
    fn record(&mut self, kind: ErrorKind, at_ms: i64) {
        let second = at_ms.div_euclid(1000);
        let slot = &mut self.slots[second.rem_euclid(ERROR_WINDOW_SECS) as usize];
        if slot.0 < second {
            *slot = (second, [0; ErrorKind::ALL.len()]);
        }
        // A slot already holding a LATER second means this one is a whole window old.
        if slot.0 == second {
            slot.1[kind.index()] = slot.1[kind.index()].saturating_add(1);
        }
        if self.last.is_none_or(|(_, last_ms)| at_ms >= last_ms) {
            self.last = Some((kind, at_ms));
        }
    }

    /// Errors of `kind` in the ten minutes ending at `now_ms`: the seconds in
    /// `(now - 600s, now]`.
    fn count(&self, kind: ErrorKind, now_ms: i64) -> u32 {
        let now_second = now_ms.div_euclid(1000);
        self.slots
            .iter()
            .filter(|(second, _)| *second > now_second - ERROR_WINDOW_SECS && *second <= now_second)
            .map(|(_, counts)| counts[kind.index()])
            .fold(0, u32::saturating_add)
    }
}

/// What this boot has observed that the document reports per provider: upstream errors and
/// the model ids served. The [`Manager`] holds it; nothing in selection reads it.
#[derive(Debug, Default)]
pub struct Observations {
    errors: [ErrorLog; 2],
    models: [BTreeSet<String>; 2],
}

impl Observations {
    /// Note an upstream error of `kind` on `provider` at `at_ms`.
    pub fn record_error(&mut self, provider: Provider, kind: ErrorKind, at_ms: i64) {
        self.errors[slot(provider)].record(kind, at_ms);
    }

    /// Note that `provider` served model `id`.
    pub fn record_model(&mut self, provider: Provider, id: &str) {
        let ids = &mut self.models[slot(provider)];
        if !ids.contains(id) && ids.len() < MODEL_IDS_CAP {
            ids.insert(id.to_owned());
        }
    }

    /// The last error on `provider` as `(kind, at_ms, count of that kind in the ten minutes
    /// ending at now_ms)`, or `None` when none was seen this boot.
    pub fn last_error(&self, provider: Provider, now_ms: i64) -> Option<(ErrorKind, i64, u32)> {
        let log = &self.errors[slot(provider)];
        log.last
            .map(|(kind, at_ms)| (kind, at_ms, log.count(kind, now_ms)))
    }

    /// Every model id `provider` served this boot.
    pub fn model_ids(&self, provider: Provider) -> BTreeSet<String> {
        self.models[slot(provider)].clone()
    }
}

/// Each provider's place in [`Observations`].
const fn slot(provider: Provider) -> usize {
    match provider {
        Provider::Anthropic => 0,
        Provider::Codex => 1,
    }
}

/// The path a new inference request of `provider` arrives on, the one its selection is asked
/// about.
const fn inference_path(provider: Provider) -> &'static str {
    match provider {
        Provider::Anthropic => "/v1/messages",
        Provider::Codex => crate::provider::CODEX_RESPONSES_PATH,
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
        forecast: None,
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
/// family, in [`crate::model::FAMILIES`] order, an id naming none inventing no family; a
/// `codex` id as its own model.
fn catalogue(provider: Provider, seen: &BTreeSet<String>) -> Vec<(String, Vec<String>)> {
    match provider {
        Provider::Anthropic => crate::model::FAMILIES
            .iter()
            .filter_map(|&family| {
                let ids: Vec<String> = seen
                    .iter()
                    .filter(|id| crate::model::family(id) == Some(family))
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
    let hint = manager.exhaustion_hint(now, found.is_fable, provider, None);
    let gates = match provider {
        Provider::Anthropic => crate::proxy::describe_gates(&hint.gated),
        Provider::Codex => crate::proxy::describe_codex_gates(&hint.gated),
    };
    let frees_at = match hint.free_at {
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

    /// A fixed instant on a whole second, in ms.
    const AT: i64 = 1_800_000_000_000;

    /// **`count10m` counts the last error's kind in the ten minutes up to now, per
    /// provider, and only those.**
    #[test]
    fn count10m_counts_the_last_kind_in_the_ten_minutes_up_to_now() {
        let mut seen = Observations::default();
        // Exactly ten minutes before `AT` is outside `(now - 600s, now]`.
        seen.record_error(Provider::Anthropic, ErrorKind::Quota, AT - 600_000);
        seen.record_error(Provider::Anthropic, ErrorKind::Quota, AT - 599_000);
        seen.record_error(Provider::Anthropic, ErrorKind::Transport, AT - 1_000);
        seen.record_error(Provider::Anthropic, ErrorKind::Quota, AT - 500);
        seen.record_error(Provider::Codex, ErrorKind::Quota, AT - 400);
        assert_eq!(
            seen.last_error(Provider::Anthropic, AT),
            Some((ErrorKind::Quota, AT - 500, 2)),
            "the last error is quota, and two quota errors fell inside the window"
        );
        assert_eq!(
            seen.last_error(Provider::Codex, AT),
            Some((ErrorKind::Quota, AT - 400, 1)),
            "each provider keeps its own errors"
        );
        assert_eq!(
            seen.last_error(Provider::Anthropic, AT + 600_000),
            Some((ErrorKind::Quota, AT - 500, 0)),
            "ten minutes on, the last error is still named and counts nothing"
        );
        assert_eq!(
            Observations::default().last_error(Provider::Codex, AT),
            None
        );
    }

    /// **The window is fixed memory**: a burst over far longer than ten minutes keeps one
    /// slot per second, and an error a whole window older than its slot is not counted.
    #[test]
    fn the_error_window_is_bounded_and_forgets_what_it_overwrites() {
        let mut log = ErrorLog::default();
        for second in 0..3_000 {
            log.record(ErrorKind::Overloaded, AT + second * 1_000);
        }
        assert_eq!(log.slots.len(), 600);
        assert_eq!(log.count(ErrorKind::Overloaded, AT + 2_999_000), 600);
        log.record(ErrorKind::Overloaded, AT);
        assert_eq!(
            log.count(ErrorKind::Overloaded, AT + 2_999_000),
            600,
            "a late error a window old lands on a slot a later second holds"
        );
        assert_eq!(log.last, Some((ErrorKind::Overloaded, AT + 2_999_000)));
    }

    /// **A status alone names auth, quota and overloaded, and nothing else.** A 403 is
    /// recorded only where it refuses the credential.
    #[test]
    fn a_status_names_only_the_kinds_it_can() {
        assert_eq!(ErrorKind::of_status(401), Some(ErrorKind::Auth));
        assert_eq!(ErrorKind::of_status(429), Some(ErrorKind::Quota));
        assert_eq!(ErrorKind::of_status(529), Some(ErrorKind::Overloaded));
        assert_eq!(ErrorKind::of_status(503), Some(ErrorKind::Overloaded));
        for status in [200, 400, 403, 404, 500, 502] {
            assert_eq!(ErrorKind::of_status(status), None, "{status}");
        }
    }

    /// **A failed stream names transport when it proved truncated and overloaded when it
    /// failed in-band as overloaded; any other in-band error names none.**
    #[test]
    fn a_stream_names_transport_or_overloaded_and_nothing_else() {
        assert_eq!(
            ErrorKind::of_stream("truncated"),
            Some(ErrorKind::Transport)
        );
        assert_eq!(
            ErrorKind::of_stream("overloaded_error"),
            Some(ErrorKind::Overloaded)
        );
        assert_eq!(
            ErrorKind::of_stream("server_is_overloaded"),
            Some(ErrorKind::Overloaded)
        );
        assert_eq!(ErrorKind::of_stream("context_length_exceeded"), None);
        assert_eq!(ErrorKind::of_stream("unknown"), None);
    }

    /// **Every served model id is remembered per provider, including later families.**
    #[test]
    fn model_ids_are_kept_per_provider_without_truncation() {
        let mut seen = Observations::default();
        seen.record_model(Provider::Codex, "gpt-a");
        seen.record_model(Provider::Codex, "gpt-a");
        assert_eq!(seen.model_ids(Provider::Codex).len(), 1);
        assert!(seen.model_ids(Provider::Anthropic).is_empty());
        for n in 0..100 {
            seen.record_model(Provider::Anthropic, &format!("claude-opus-{n}"));
        }
        assert_eq!(seen.model_ids(Provider::Anthropic).len(), 100);
        seen.record_model(Provider::Anthropic, "claude-sonnet-5-5");
        assert!(
            seen.model_ids(Provider::Anthropic)
                .contains("claude-sonnet-5-5")
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
