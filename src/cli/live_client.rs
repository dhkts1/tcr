//! The one HTTP client for the running proxy's local routes: the loopback transport, the route
//! answer classifier, the status read, the add-account call, and the incumbent probe.

use std::time::Duration;

use crate::build_info::BuildInfo;
use crate::config::{Account, Config};
use crate::provider::Provider;
use crate::status::{STATUS_KIND, StatusPayload};

/// The last sentence of every "proxy too old for live control" warning: how to
/// restart the proxy so it re-reads the config file it was just written.
///
/// There is no restart subcommand, so the hint names the two real routes:
///
/// - TcrBar's Settings > General > Restart, when TcrBar supervises the proxy.
///   It stops the child it spawned and starts a new one
///   (`apps/macos/Sources/TcrBar/SettingsPanes.swift`, `confirmRestart`). A
///   terminal `--replace` would instead kill that child and leave the proxy
///   tied to a terminal window.
/// - `tcr server --replace` otherwise: the singleton kills the proxy holding
///   the port and binds in its place (`ServerArgs::replace`, docs/cli.md
///   "Taking over the port").
///
/// One copy, because the four warnings that print it all named a `restart`
/// subcommand that never existed, and four copies is how that happened. `main.rs`'s `restart_hint_names_only_commands_that_parse` parses
/// every backticked `tcr` command in this text with the real clap definition.
pub const RESTART_HINT: &str = concat!(
    "Restart it when a cold prompt cache is acceptable: if TcrBar runs the proxy, use Restart in TcrBar's Settings > General; otherwise stop it, or use `tcr server --replace` if it runs tcr ",
    crate::handoff_since!(),
    " or newer."
);

/// Why a live account-control request did not apply.
///
/// The arms are the CLI's decision table, not decoration: one of them must NOT
/// write the config (`Unauthorized`), one writes it quietly (`NoServer`), and the
/// rest write it and shout. Collapsing them is how a half-applied disable becomes
/// invisible again.
///
/// `pub(crate)`: [`crate::oauth`]'s live-login half reuses this exact enum for
/// [`post_add_account`] rather than defining a parallel one — see that
/// function's doc-comment.
#[derive(Debug)]
pub(crate) enum LiveControlError {
    /// Nothing is listening on the configured port. No live rotation exists, so
    /// the file IS the whole truth — the ordinary offline case.
    NoServer,
    /// A server answered, but the account-control route is not there: an older
    /// tcr, identified structurally by the absent
    /// [`crate::proxy::ENDPOINT_HEADER`] on a 404/405, never by error text.
    NoRoute,
    /// The proxy rejected our api-key. The one arm that must not touch the file.
    Unauthorized,
    /// Something is listening but produced no usable response before the deadline.
    NoAnswer(String),
    /// It answered and the answer was not one we can act on: a 4xx/5xx from the
    /// route itself, or a body that is not ours.
    Unusable(String),
    /// The route resolved our query to nothing, or to more than one account. The
    /// server's own message is carried through verbatim — it names the candidates.
    Rejected(String),
}

impl LiveControlError {
    pub(crate) fn why(&self) -> String {
        match self {
            Self::NoServer => "nothing is listening".to_string(),
            Self::NoRoute => "the running proxy has no account-control route".to_string(),
            Self::Unauthorized => "the proxy api-key was rejected".to_string(),
            Self::NoAnswer(why) | Self::Unusable(why) | Self::Rejected(why) => why.clone(),
        }
    }
}

impl From<Unreached> for LiveControlError {
    fn from(unreached: Unreached) -> Self {
        match unreached {
            Unreached::Refused => Self::NoServer,
            Unreached::Silent(why) => Self::NoAnswer(why),
            Unreached::Unbuilt(why) => Self::Unusable(why),
        }
    }
}

/// Why a call to the running proxy came back with no HTTP response at all.
#[derive(Debug)]
pub(super) enum Unreached {
    /// The connection was refused: nothing is listening on the port.
    Refused,
    /// Something holds the port and sent nothing back in time, or dropped the
    /// connection mid-read.
    Silent(String),
    /// The HTTP client could not be built, so nothing was sent.
    Unbuilt(String),
}

/// Sort a failed send. `is_timeout()` comes first: a connect TIMEOUT (a
/// blackholed address, or a SYN dropped rather than refused) reports
/// `is_connect() == true` as well, so asking `is_connect()` first would call a
/// wedged proxy "nothing is listening" when the truer reading is "something is
/// there but not answering".
fn unreached(e: &reqwest::Error) -> Unreached {
    if e.is_timeout() {
        Unreached::Silent("the server did not answer within 5s".to_string())
    } else if e.is_connect() {
        Unreached::Refused
    } else {
        Unreached::Silent(e.to_string())
    }
}

/// Send one request to the running proxy at `path` on 127.0.0.1 and the
/// configured port. Every call to a `/_tcr/` route goes through here.
///
/// The client never honours a system proxy: `HTTP_PROXY` commonly points AT
/// tcr, so honouring it would send this request through the proxy it is about.
/// The api-key goes on every request, because no local route has a loopback
/// exemption. 2s to connect, 5s in total.
pub(super) async fn send_loopback(
    config: &Config,
    path: &str,
    build: impl FnOnce(&reqwest::Client, &str) -> reqwest::RequestBuilder,
) -> Result<reqwest::Response, Unreached> {
    send_loopback_with_redirect_policy(config, path, reqwest::redirect::Policy::default(), build)
        .await
}

/// The loopback transport with an explicit redirect policy for reads that must stay
/// on the configured endpoint. Other callers retain the client's default policy.
pub(super) async fn send_loopback_with_redirect_policy(
    config: &Config,
    path: &str,
    redirects: reqwest::redirect::Policy,
    build: impl FnOnce(&reqwest::Client, &str) -> reqwest::RequestBuilder,
) -> Result<reqwest::Response, Unreached> {
    let client = crate::http_client::builder()
        .redirect(redirects)
        .no_proxy()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| Unreached::Unbuilt(format!("http client: {e}")))?;
    let url = format!("http://127.0.0.1:{}{path}", config.proxy.port);
    let mut request = build(&client, &url);
    if let Some(key) = config.proxy.api_key.as_deref() {
        request = request.header("x-api-key", key);
    }
    request.send().await.map_err(|e| unreached(&e))
}

/// One local mutating route on the running proxy, as data: where it is, the
/// [`crate::proxy::ENDPOINT_HEADER`] value it stamps on its answers, and which
/// stamped statuses mean "the route refused this request".
pub(super) struct ControlRoute {
    pub(super) path: &'static str,
    pub(super) endpoint: &'static str,
    /// Stamped statuses that are the route saying no, carried to the caller as
    /// [`LiveControlError::Rejected`] with the server's own message. Per route,
    /// because each route refuses with its own codes: 404 is "no such account"
    /// on the account routes, 400 is request validation on the add and settings
    /// routes, and 409 is an ambiguous match everywhere.
    pub(super) refusals: &'static [u16],
    /// What the 2xx body is, for the error when it does not parse.
    pub(super) noun: &'static str,
    /// Whether a 2xx counts only when the route stamped it.
    pub(super) success_needs_stamp: bool,
}

/// 400 is the route's own request validation (a blank name or token), the
/// positive capability signal `oauth::probe_add_route` looks for; 409 is a
/// submitted identity that matched more than one live account.
const ADD_ACCOUNT_ROUTE: ControlRoute = ControlRoute {
    path: crate::proxy::ADD_ACCOUNT_PATH,
    endpoint: crate::proxy::ADD_ACCOUNT_ENDPOINT,
    refusals: &[400, 409],
    noun: "account-add",
    success_needs_stamp: false,
};

/// What [`call_route`] read: the sorted answer, and the response headers when
/// the route stamped the answer, so a caller can read a second header off the
/// same response with no second round trip.
pub(super) struct RouteAnswer<T> {
    pub(super) result: Result<T, LiveControlError>,
    stamped: Option<reqwest::header::HeaderMap>,
    pub(super) status: Option<u16>,
}

/// Send one request to `route` and sort the answer.
///
/// Whether THIS route produced the answer is decided ONLY by
/// [`crate::proxy::ENDPOINT_HEADER`], never by status code or error text: a 404
/// from a tcr without the route and a 404 meaning "no such account" are the same
/// status with opposite consequences, and the header is the only difference that
/// is not a matched error string. So an unstamped 404 or 405 is
/// [`LiveControlError::NoRoute`], and a stamped refusal status is
/// [`LiveControlError::Rejected`] carrying the server's message.
pub(super) async fn call_route<T: serde::de::DeserializeOwned>(
    config: &Config,
    route: &ControlRoute,
    build: impl FnOnce(&reqwest::Client, &str) -> reqwest::RequestBuilder,
) -> RouteAnswer<T> {
    call_route_with_policy(config, route, reqwest::redirect::Policy::default(), build).await
}

/// One response classifier; callers choose redirect policy without duplicating transport.
pub(super) async fn call_route_with_policy<T: serde::de::DeserializeOwned>(
    config: &Config,
    route: &ControlRoute,
    redirects: reqwest::redirect::Policy,
    build: impl FnOnce(&reqwest::Client, &str) -> reqwest::RequestBuilder,
) -> RouteAnswer<T> {
    let response =
        match send_loopback_with_redirect_policy(config, route.path, redirects, build).await {
            Ok(response) => response,
            Err(unreached) => {
                return RouteAnswer {
                    result: Err(unreached.into()),
                    stamped: None,
                    status: None,
                };
            }
        };
    let status = response.status();
    let from_route = response
        .headers()
        .get(crate::proxy::ENDPOINT_HEADER)
        .and_then(|v| v.to_str().ok())
        == Some(route.endpoint);
    let stamped = from_route.then(|| response.headers().clone());
    let body = response.text().await;
    let message = || body.as_deref().ok().and_then(error_message_of);

    let result = if status.is_success() && (from_route || !route.success_needs_stamp) {
        match &body {
            Ok(body) => serde_json::from_str(body).map_err(|e| {
                LiveControlError::Unusable(format!(
                    "the response was not a tcr {} payload ({e})",
                    route.noun
                ))
            }),
            Err(e) => Err(LiveControlError::Unusable(format!(
                "reading the answer: {e}"
            ))),
        }
    } else if status.as_u16() == 401 {
        Err(LiveControlError::Unauthorized)
    } else if !from_route && matches!(status.as_u16(), 404 | 405) {
        Err(LiveControlError::NoRoute)
    } else if from_route && route.refusals.contains(&status.as_u16()) {
        Err(LiveControlError::Rejected(
            message().unwrap_or_else(|| format!("HTTP {status}")),
        ))
    } else {
        Err(LiveControlError::Unusable(format!(
            "HTTP {status}{}",
            message().map_or_default(|m| format!(": {m}"))
        )))
    };
    RouteAnswer {
        result,
        stamped,
        status: Some(status.as_u16()),
    }
}

/// Ask the running proxy to add an account to the live rotation, or — when the
/// submitted identity already matches one — replace its credentials in place.
/// No restart, so no session pin is invalidated.
///
/// `account` is `POSTed` directly as the request body: [`config::Account`]
/// already serializes to the exact wire shape
/// [`crate::proxy::ADD_ACCOUNT_PATH`] expects (see that route's
/// `AddAccountRequest` doc-comment), so a caller builds the credential once —
/// for a real login, or `crate::oauth::probe_add_route`'s deliberately
/// blank probe — and this function never re-lists its fields.
///
/// A stamped `400` is this route's own request-validation failure (a blank
/// name or token), and reads as [`LiveControlError::Rejected`] like a stamped
/// `409`; see [`ADD_ACCOUNT_ROUTE`].
pub(crate) async fn post_add_account(
    config: &Config,
    account: &Account,
) -> Result<crate::proxy::AddAccountResponse, LiveControlError> {
    post_add_account_with_providers(config, account).await.0
}

/// [`post_add_account`], plus the providers the answering server advertised in
/// [`crate::proxy::PROVIDERS_HEADER`] — read off the SAME response, so a caller
/// that must know whether the server can store a non-Anthropic account learns it
/// from the probe it already sends, with no second round trip.
///
/// The list is read only from an answer the route STAMPED: an unstamped one (an
/// older tcr's catch-all, the gate's refusal, something that is not tcr at all) is
/// no proof of anything, and neither is a request that never got an answer. Both
/// come back as an empty list, which is how "this server never said" reads.
pub(crate) async fn post_add_account_with_providers(
    config: &Config,
    account: &Account,
) -> (
    Result<crate::proxy::AddAccountResponse, LiveControlError>,
    Vec<Provider>,
) {
    let answer: RouteAnswer<crate::proxy::AddAccountResponse> =
        call_route(config, &ADD_ACCOUNT_ROUTE, |client, url| {
            client.post(url).json(account)
        })
        .await;
    let providers = answer
        .stamped
        .as_ref()
        .and_then(|headers| headers.get(crate::proxy::PROVIDERS_HEADER))
        .and_then(|v| v.to_str().ok())
        .map_or_default(crate::proxy::providers_in_header);
    (answer.result, providers)
}

/// Pull `error.message` out of the proxy's standard error envelope.
fn error_message_of(body: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()?
        .get("error")?
        .get("message")?
        .as_str()
        .map(str::to_string)
}

/// Why a live status read did not produce a payload.
///
/// The three variants are not interchangeable, and collapsing them is what made
/// a wedged proxy unrecoverable: [`probe_incumbent`] turns them into
/// "is anything actually serving on this port", and that answer is the
/// difference between "leave a working proxy alone" and "this process is holding
/// the socket and answering nothing".
///
/// Public for `tcr run --codex`, which pools a session only on what the
/// RUNNING proxy reports, and says which of these stopped it when it cannot.
#[derive(Debug)]
pub enum LiveStatusError {
    /// Nothing is listening on the configured port — the ORDINARY case (`tcr
    /// status` with no server running). Falling back is expected, so it is
    /// reported by the `offline` label alone rather than by a warning.
    NoServer,
    /// Something is listening but produced no response before the deadline: no
    /// bytes back within the 2s connect / 5s total budget, or the connection
    /// dropped mid-read. This is the WEDGED shape.
    NoAnswer(String),
    /// A server answered — an HTTP response came back — but the read was not
    /// usable: a rejected api-key, an older tcr with no status route, a payload
    /// that is not ours. The process is SERVING; we just cannot read it. Always
    /// warned about: a silently-swallowed rejection here would look exactly like
    /// "no server", which is how an api-key typo becomes a mysterious all-zero
    /// status.
    Unusable(String),
}

impl From<Unreached> for LiveStatusError {
    fn from(unreached: Unreached) -> Self {
        match unreached {
            Unreached::Refused => Self::NoServer,
            Unreached::Silent(why) => Self::NoAnswer(why),
            Unreached::Unbuilt(why) => Self::Unusable(why),
        }
    }
}

impl LiveStatusError {
    /// Did an HTTP response come back at all?
    ///
    /// The load-bearing distinction for the startup stand-down. A 401 or an
    /// unparseable body is a process that ACCEPTED the connection, routed the
    /// request and wrote a response — the opposite of wedged — while a timeout
    /// means the listening socket is all that is left of it.
    pub(super) const fn answered(&self) -> bool {
        matches!(self, Self::Unusable(_))
    }

    /// The failure in a few words, for an operator-facing line.
    pub fn why(&self) -> String {
        match self {
            Self::NoServer => "nothing is listening".to_string(),
            Self::NoAnswer(why) | Self::Unusable(why) => why.clone(),
        }
    }
}

/// Whether the incumbent holding the port is actually serving anything.
///
/// Deliberately NOT `Option<BuildInfo>::is_none()`. That conflates four states,
/// two of which are a perfectly healthy proxy: an api-key mismatch and an older
/// `tcr` with no status route both answer, and killing either would be a
/// takeover of a live server on the strength of a diagnostic read failing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Liveness {
    /// It answered. Whatever else is true, it is serving requests.
    Answering,
    /// Nothing came back before the deadline. It holds the socket and serves
    /// nothing; `why` is the probe's own account of what happened, for the
    /// operator-facing line.
    Silent { why: String },
}

/// Read the live fleet snapshot from the running proxy's [`crate::proxy::STATUS_PATH`].
///
/// Sends the configured proxy api-key: unlike the forwarding path, the status
/// endpoint has no loopback exemption. Only a body that deserializes AND names
/// the exact [`STATUS_KIND`] is accepted — a tcr built before the endpoint
/// existed has no such route, so it forwards this path UPSTREAM and hands back
/// Anthropic's own error JSON, which must never be rendered as a fleet status.
pub async fn fetch_live_status(config: &Config) -> Result<StatusPayload, LiveStatusError> {
    parse_live_status(&fetch_live_status_body(config).await?)
}

/// The typed half of [`fetch_live_status`], split out so a caller that needs
/// the RAW body as well (`sessions()`, which must tell an absent `sessions`
/// key from an empty array — `#[serde(default)]` erases the difference) can
/// read both from one request rather than issuing a second one.
pub(super) fn parse_live_status(body: &str) -> Result<StatusPayload, LiveStatusError> {
    let payload: StatusPayload = serde_json::from_str(body).map_err(|e| {
        LiveStatusError::Unusable(format!(
            "the response was not a tcr status payload ({e}) — an older tcr forwards this path upstream"
        ))
    })?;
    if payload.kind != STATUS_KIND {
        return Err(LiveStatusError::Unusable(format!(
            "unexpected payload kind '{}' (expected '{STATUS_KIND}')",
            payload.kind
        )));
    }
    Ok(payload)
}

/// The transport half of [`fetch_live_status`]: one GET, every failure mode
/// classified, the body handed back unparsed.
pub(super) async fn fetch_live_status_body(config: &Config) -> Result<String, LiveStatusError> {
    let response = send_loopback(config, crate::proxy::STATUS_PATH, |client, url| {
        client.get(url)
    })
    .await?;

    let status = response.status();
    if !status.is_success() {
        let hint = if status.as_u16() == 401 {
            " — the proxy api-key in the config was rejected"
        } else {
            ""
        };
        return Err(LiveStatusError::Unusable(format!("HTTP {status}{hint}")));
    }

    response
        .text()
        .await
        .map_err(|e| LiveStatusError::Unusable(format!("reading the response body: {e}")))
}

/// One probe of the incumbent on the port, answering BOTH questions the startup
/// stand-down has to decide on.
///
/// * `build` — which commit it is executing, so the user learns whether the
///   binary they just compiled is the one serving. `None` on any failure to read
///   it; this is a diagnostic on a path that is already exiting.
/// * `liveness` — whether it responded AT ALL. Separate from `build` on purpose:
///   `build == None` is true for a healthy proxy behind a rejected api-key and
///   for one wedged solid, and the stand-down must not treat those alike.
///
/// One probe, not two, so the decision and the message can never disagree about
/// what the incumbent did. Bounded by [`fetch_live_status`]'s own 2s connect /
/// 5s total timeouts, so it cannot hang.
pub struct IncumbentProbe {
    pub build: Option<BuildInfo>,
    pub liveness: Liveness,
}

pub async fn probe_incumbent(config: &Config) -> IncumbentProbe {
    match fetch_live_status(config).await {
        Ok(payload) => IncumbentProbe {
            build: Some(payload.build),
            liveness: Liveness::Answering,
        },
        Err(err) => IncumbentProbe {
            build: None,
            liveness: if err.answered() {
                Liveness::Answering
            } else {
                Liveness::Silent { why: err.why() }
            },
        },
    }
}
