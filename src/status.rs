//! The wire contract between the running proxy's read-only status endpoint
//! ([`crate::proxy::STATUS_PATH`]) and `tcr status`.
//!
//! # Why it exists
//!
//! `tcr status` used to build a FRESH offline [`Manager`](crate::manager::Manager)
//! and print its counters. Those counters live in the *server's* process, so the
//! offline ones are structurally zero — which made `cacheHitRatio` report a
//! confident `0.0` for every account forever. A metric that cannot fire is worse
//! than no metric: it reported "cache fine" straight through a real prompt-cache
//! catastrophe. This module is the seam that lets the CLI read the *live*
//! process's numbers instead.
//!
//! # The no-secret invariant
//!
//! The proxy holds every account's OAuth access **and** refresh token. This
//! payload is an explicit projection of [`AccountSnapshot`] — the display struct
//! the TUI paints — which by construction carries no credential material: no
//! access token, no refresh token, no proxy api-key, no `Authorization` echo.
//! Two things keep it that way:
//!
//! 1. [`StatusPayload::into_snapshot`] rebuilds `AccountSnapshot` with a struct
//!    literal that names **every** field, so adding a field to `AccountSnapshot`
//!    is a compile error here — a mechanical prompt to decide, deliberately,
//!    whether it may cross a process boundary.
//! 2. `status_endpoint_leaks_no_secrets` (in `proxy.rs`) asserts on the response
//!    **bytes**, not on a struct, so a leak introduced through any path fails a
//!    test rather than a code review.
//!
//! Deliberately NOT on the wire: the recent-request ring buffer (it carries the
//! request paths a client sent) and the affinity-hash session table
//! ([`StatsSnapshot::sessions`], which exists for the TUI's pin display). `tcr status` prints
//! neither, so neither is exposed.
//!
//! [`StatsSnapshot::wire_sessions`] — the Claude Code `session_id`-keyed table with tool-call
//! timing (F1, `docs/design/panel-tabs.md`) — IS on the wire, as [`StatusPayload::sessions`]:
//! it carries no credential material either, and it is the whole point of the feature.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

pub mod network_fact;
mod peer_graph;
mod peer_ls;
mod peer_rows;

use crate::build_info::BuildInfo;
use crate::probe::ProbeStatus;
use crate::stats::{AccountSnapshot, CodexFacts, GateReason, QuotaState, StatsSnapshot};

/// Discriminator carried by every status response, checked by the client before
/// it trusts a body.
///
/// It exists because of version skew: a tcr server built *before* this endpoint
/// existed has no `/_tcr/status` route, so the request falls through to its
/// catch-all and is forwarded to Anthropic — whose error JSON would otherwise be
/// a plausible-looking body. Requiring an exact `kind` match means only a payload
/// this code produced is ever rendered as live status; anything else falls back
/// to the offline snapshot with a visible warning. Bump the suffix if the shape
/// ever changes incompatibly.
pub const STATUS_KIND: &str = "tcr.status.v1";

/// A live fleet view as served by the proxy: one row per configured account, in
/// the server's account order, plus the build the server is running.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusPayload {
    /// Always [`STATUS_KIND`]. See its docs for why the client demands it.
    pub kind: String,
    pub accounts: Vec<AccountStatus>,
    /// Which commit the SERVING process was built from — the one fact no client
    /// can derive, since the server may have been running for days while the
    /// checkout moved on ([`crate::build_info`]).
    ///
    /// # Why this did NOT bump [`STATUS_KIND`]
    ///
    /// The client demands an EXACT `kind` match and falls back to the offline
    /// snapshot — whose serving counters are structurally zero — on any
    /// mismatch. So a bump is not a neutral version marker here: it would make
    /// every not-yet-rebuilt client reject a new server's payload and silently
    /// render zeros, which is precisely the false-zero failure this endpoint was
    /// built to end. A bump has to be reserved for a change that would otherwise
    /// be MISREAD, and this one cannot be, in either direction:
    ///
    /// * OLD client ← NEW server: serde ignores unknown fields by default, so
    ///   the extra `build` object is skipped and every field the old client
    ///   reads is unchanged. (`status_payload_tolerates_an_unknown_field` pins
    ///   that behaviour, since a later `deny_unknown_fields` would break it.)
    /// * NEW client ← OLD server: `#[serde(default)]` fills in
    ///   [`BuildInfo::default`] — every field `unknown` — and the client renders
    ///   "cannot tell whether the server is current", which is the truth.
    ///
    /// The rule this encodes: `kind` gates a change that would be MISREAD in
    /// either skew direction — never one both directions degrade through
    /// honestly. The test is behavioural, not a taxonomy of edits:
    ///
    /// * Would an OLD client reading a NEW payload render something FALSE (as
    ///   opposed to something absent)? Unknown fields are skipped by default, so
    ///   an addition passes; a field RETYPED or given new meaning under the same
    ///   name does not, because the old client parses it and believes it.
    /// * Would a NEW client reading an OLD payload render something FALSE? With
    ///   `#[serde(default)]` plus `skip_serializing_if` the field simply reads as
    ///   its default, which is the truth "the server did not report this".
    ///   WITHOUT a default it is a hard deserialize error and the client falls
    ///   back to the all-zeros offline snapshot — a fabricated healthy fleet.
    ///
    /// A field REMOVAL is therefore not automatically a bump, and an earlier
    /// wording of this rule said it was. `free_at_floor_ms` was removed without
    /// one, correctly: it was `Option` with `default` + `skip_serializing_if` on
    /// both sides and there is no `deny_unknown_fields` on this payload, so an
    /// old client sees an absent optional and a new client sees `None` — both
    /// true. Bumping there would have made every not-yet-rebuilt client reject
    /// the payload and render the structural zeros this endpoint exists to end.
    ///
    /// The real hazard is the opposite one, and it has bitten: adding a REQUIRED
    /// field. See [`AccountRow::stream_error_count`] — added without a default,
    /// it made a newer client unable to parse an older running server at all.
    #[serde(default)]
    pub build: BuildInfo,
    /// Whether the SERVING process is forcing HTTP/1.1 on its upstream
    /// clients — see [`crate::config::Config::http1_only`]. Server-wide, like
    /// `build`, and for the same reason: re-deriving it from
    /// `~/.config/tcr.json` on the client would report the file's
    /// state, not the already-booted process's, and the two can differ (the
    /// config was edited after boot; the flag is only read at client
    /// construction).
    ///
    /// `#[serde(default)]` for the same back-compat reason as `build`: an
    /// OLD server's payload has no such key, and absent must read as `false`
    /// (the actual default) rather than fail the parse and drop to the
    /// offline snapshot.
    #[serde(default)]
    pub http1_only: bool,
    /// The identity-bound control account's NAME (`Config::control_account`,
    /// resolved by `Manager::control_name`), or `None` when unset. Server-wide
    /// like `http1_only`, and for the same `#[serde(default,
    /// skip_serializing_if)]` reason: an OLD server's payload has no such key,
    /// and absent must read as "no control account reported" rather than fail
    /// the parse and drop to the offline snapshot. A NEW client reading an OLD
    /// server gets `None` — true, the server genuinely never reported one — and
    /// an OLD client reading a NEW server simply never looks at the extra key.
    /// Neither direction is MISREAD, so — per this struct's `build`
    /// doc-comment on when a bump is and is not warranted — this must NOT bump
    /// [`STATUS_KIND`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control: Option<String>,
    /// Every group on the fleet mapped to its resolved color
    /// (`crate::config::Config::group_colors`), server-wide like `http1_only`
    /// and for the same reason: it is resolved from the config the SERVING
    /// process booted with (`Manager::group_colors`), and a client must not
    /// re-derive it from a config file that may have moved on since boot.
    ///
    /// `#[serde(default)]` for the same forward-compat reason as `http1_only`
    /// — an OLD server's payload has no such key, and absent must read as an
    /// empty map (a NEW client talking to an old server reports no group
    /// colors, which is the truth: that server never resolved any) rather
    /// than fail the parse and drop to the offline snapshot. Per this
    /// struct's `build` doc-comment on when a bump is and is not warranted,
    /// this must NOT bump [`STATUS_KIND`] — same reasoning as `control`.
    #[serde(default)]
    pub group_colors: std::collections::BTreeMap<String, String>,
    /// One row per Claude Code session the proxy has seen in the last hour (F1,
    /// `docs/design/panel-tabs.md`) — see [`crate::stats::StatsSnapshot::wire_sessions`] for
    /// where this comes from and [`tcr_status_wire::SessionRow`] for the shape.
    ///
    /// `#[serde(default)]` for the same forward/back-compat reason as `group_colors` above: an
    /// OLD server's payload has no such key, and a NEW client reads an empty array — the
    /// truth, "this server never reported any sessions" — rather than failing the parse and
    /// dropping to the all-zeros offline snapshot. An OLD client reading a NEW server simply
    /// never looks at the extra key. Neither direction is MISREAD, so this must NOT bump
    /// [`STATUS_KIND`], per this struct's `build` doc-comment.
    #[serde(default)]
    pub sessions: Vec<tcr_status_wire::SessionRow>,
    /// Fleet-wide tool-call totals summed across [`Self::sessions`] server-side — see
    /// [`tcr_status_wire::SessionsSummary`]'s doc-comment. `#[serde(default)]` for the same
    /// forward/back-compat reason as `sessions` itself: an OLD server's payload has no such
    /// key, and a NEW client reads the all-zero default — the truth, "this server never
    /// reported one" — rather than failing the parse and dropping to the offline snapshot.
    /// Neither skew direction is MISREAD, so this must NOT bump [`STATUS_KIND`].
    #[serde(default)]
    pub sessions_summary: tcr_status_wire::SessionsSummary,
    /// One row per pinned Mac, in the vocabulary the panel already decodes,
    /// see [`PeerStatusRow`], and [`peer_status_rows`] for the derivation.
    ///
    /// # Why the peers ride the STATUS payload at all
    ///
    /// The Peers tab used to answer its live half from `tcr peer ls --json`,
    /// which is a projection of two FILES. Thirteen of the sixteen fields the
    /// panel decodes had no Rust writer at all, so the tab drew placeholders
    /// while both suites stayed green. A file cannot answer "how fast is this
    /// path right now"; the serving process can, and this is the seam that
    /// lets it.
    ///
    /// `#[serde(default, skip_serializing_if)]` for the same forward/back-compat
    /// reason as [`Self::sessions`], and, per this struct's `build`
    /// doc-comment on when a bump is and is not warranted, this must NOT bump
    /// [`STATUS_KIND`]: an OLD client skips the unknown key, and a NEW client
    /// reading an OLD server reads an empty list, which is the truth ("that
    /// server never reported any peers") rather than a fabricated mesh. A bump
    /// would make every not-yet-rebuilt client reject the payload and render
    /// the structural zeros this endpoint exists to end.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub peers: Vec<PeerStatusRow>,
    /// Why [`Self::peers`] is empty when the peers file itself could not be
    /// read (a bad edit, a permission mismatch) rather than because this Mac
    /// has never paired with anything. `None` on the ordinary paths: no
    /// peers file at all, or one that parsed fine and simply lists nobody.
    ///
    /// Follows [`AccountStatus::probe_error`]'s shape and, per this struct's
    /// `build` doc-comment on when a bump is and is not warranted, this must
    /// NOT bump [`STATUS_KIND`]: an OLD client skips the unknown key, and a
    /// NEW client reading an OLD server reads `None`, which is the truth
    /// ("that server never reported a read failure") rather than a
    /// fabricated one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peers_error: Option<String>,
    /// What the SERVING process's MITM listener can terminate, decided once at
    /// boot from its policy and the TLS material it loaded
    /// ([`MitmStatus::intercepted_hosts`] says what `chatgpt.com` there means).
    /// Nothing reloads that material, so a client reading the leaf off disk can
    /// disagree with the running process; this field is the running process's
    /// own answer. `tcr run --codex` pools a session only when it names
    /// `chatgpt.com` and the account rows hold a Codex account.
    ///
    /// Tri-state on purpose, and the states mean different things: `None` is
    /// an OLD server, one that predates the key and so cannot say what it
    /// intercepts, while a server that has the key ALWAYS sends `Some`, MITM
    /// off included (`leaf: "none"`). A `#[serde(default)]` struct or
    /// list would make "old server" and "MITM off" read the same, and only the
    /// first is fixed by restarting on a current build. `from_snapshot` leaves
    /// it `None` and the serving process fills it, as with [`Self::peers`].
    ///
    /// `#[serde(default, skip_serializing_if)]` and, per this struct's `build`
    /// doc-comment on when a bump is and is not warranted, NOT a
    /// [`STATUS_KIND`] bump: an OLD client skips the unknown key, and a NEW
    /// client reading an OLD server reads `None`, which is the truth ("that
    /// server reports no interception").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mitm: Option<MitmStatus>,
    /// What the serving process did with Codex requests in the last ten minutes:
    /// the 429s it sent itself, the latest one's error `type` and when, and the
    /// requests it relayed on the client's own credential. A fleet-wide fact the
    /// CLI copies onto every Codex row of `tcr status --json`. `from_snapshot`
    /// leaves it `None` and the serving process fills it, as with [`Self::mitm`].
    ///
    /// `#[serde(default, skip_serializing_if)]` and, per this struct's `build`
    /// doc-comment on when a bump is and is not warranted, NOT a
    /// [`STATUS_KIND`] bump: an OLD client skips the unknown key, and a NEW
    /// client reading an OLD server reads `None`, which is the truth ("that
    /// server predates it").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_traffic: Option<tcr_status_wire::CodexTraffic>,
    /// Where the serving process sends a Codex account's requests, the boot
    /// snapshot of `codexUpstream`, sent only when it is not the default
    /// (`https://chatgpt.com`). `None` is the default, or an OLD server that
    /// predates the key; either way the CLI names no other upstream.
    /// `from_snapshot` leaves it `None` and the serving process fills it, as
    /// with [`Self::mitm`].
    ///
    /// `#[serde(default, skip_serializing_if)]` and, like [`Self::mitm`], NOT a
    /// [`STATUS_KIND`] bump: an OLD client skips the unknown key, and a NEW
    /// client reading an OLD server reads `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_upstream: Option<String>,
    /// How the serving process treats a caller without its request key
    /// ([`crate::request_key::RequestKeyMode`]): `"log"` whenever it holds a
    /// key, which a server writes at boot. Its PRESENCE is the contract a
    /// launcher reads before it adds the key to a launch ("this server
    /// understands keys"), so the name is fixed. `None` is a server that holds
    /// no key or predates them.
    ///
    /// A string rather than the enum, for the reason [`MitmStatus::leaf`] gives:
    /// a value added later must not fail an older client's whole parse.
    #[serde(
        rename = "requestKeyMode",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub request_key_mode: Option<String>,
    /// `CONNECT`s that arrived without the request key, by host class and
    /// user-agent family. Sent beside [`Self::request_key_mode`] and only with it.
    #[serde(
        rename = "unkeyedConnects",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub unkeyed_connects: Option<Vec<UnkeyedCount>>,
    /// Plain requests that arrived without the request key, by path class and
    /// user-agent family. A request decrypted from a tunnel is not here: its
    /// `CONNECT` was counted.
    #[serde(
        rename = "unkeyedRequests",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub unkeyed_requests: Option<Vec<UnkeyedCount>>,
    /// How hot the serving Mac runs and how hard its fans work ([`crate::heat`]).
    /// `None` where there is no AppleSMC to read (a VM, Linux), and from an OLD
    /// server that predates the key. `from_snapshot` leaves it `None` and the
    /// serving process fills it, as with [`Self::mitm`].
    ///
    /// Not a secret: a temperature and some fan speeds, so the module's
    /// no-secret invariant holds. `#[serde(default, skip_serializing_if)]` and,
    /// per this struct's `build` doc-comment, NOT a [`STATUS_KIND`] bump: an OLD
    /// client skips the unknown key, and a NEW client reading an OLD server
    /// reads `None`, which draws the line without heat, the truth.
    ///
    /// A [`HeatField`], not a bare row, so a `heat` object this build cannot
    /// read costs the heat and nothing else: a typed field that failed would
    /// fail the whole payload, and `tcr status` would fall back to the offline
    /// snapshot over a temperature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heat: Option<HeatField>,
    /// What the serving proxy's fan controller is doing ([`crate::fan`]),
    /// a sibling of [`Self::heat`]. `None` from a process that runs no fan
    /// control (a library caller, a test) and from a server older than the
    /// key; not a [`STATUS_KIND`] bump, for the reasons [`Self::heat`] gives.
    /// A [`FanControlField`], so an object this build cannot read costs fan
    /// control and nothing else.
    #[serde(
        rename = "fanControl",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub fan_control: Option<FanControlField>,
}

/// A status field as a client read it: the value, or why it did not parse (a
/// state this build has no name for, from a newer server). The serving process
/// only ever writes [`Self::Reading`]; only a client holds [`Self::Unreadable`].
#[derive(Debug, Clone, PartialEq)]
pub enum Lenient<T> {
    Reading(T),
    /// The object did not parse as a `T`; the parser's own words. The caller
    /// says so where its diagnostics go.
    Unreadable(String),
}

/// One session's requests in the newest `minutes` of its sparkline
/// ([`tcr_status_wire::SessionRow::req_per_minute`]); a sparkline shorter than
/// the window counts what it has.
fn session_requests_in_window(session: &tcr_status_wire::SessionRow, minutes: usize) -> u64 {
    let minutes = session.req_per_minute.len().min(minutes);
    session
        .req_per_minute
        .iter()
        .rev()
        .take(minutes)
        .map(|&requests| u64::from(requests))
        .sum::<u64>()
}

/// Why a [`Lenient`] status field holds no reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unread<'a> {
    /// The server sent no such object: one older than the key, or one with nothing to report.
    NotSent,
    /// The server sent one this build could not read; the parser's own words.
    Unparsed(&'a str),
}

impl<T> Lenient<T> {
    /// `field`'s reading, or why it holds none.
    const fn read(field: Option<&Self>) -> Result<&T, Unread<'_>> {
        match field {
            Some(Self::Reading(row)) => Ok(row),
            Some(Self::Unreadable(why)) => Err(Unread::Unparsed(why.as_str())),
            None => Err(Unread::NotSent),
        }
    }
}

impl<T: Serialize> Serialize for Lenient<T> {
    /// The value as itself. An unreadable one writes `null`: there is nothing
    /// to pass on.
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Reading(row) => row.serialize(serializer),
            Self::Unreadable(_) => serializer.serialize_none(),
        }
    }
}

impl<'de, T: DeserializeOwned> Deserialize<'de> for Lenient<T> {
    /// Never fails on content: any JSON value reads, and one that is not a
    /// `T` becomes [`Self::Unreadable`].
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        Ok(match serde_json::from_value(value) {
            Ok(row) => Self::Reading(row),
            Err(error) => Self::Unreadable(error.to_string()),
        })
    }
}

/// [`StatusPayload::fan_control`] as a client read it.
pub type FanControlField = Lenient<tcr_status_wire::FanControlRow>;
/// [`StatusPayload::heat`] as a client read it.
pub type HeatField = Lenient<tcr_status_wire::HeatRow>;

/// One row of [`StatusPayload::unkeyed_connects`] or
/// [`StatusPayload::unkeyed_requests`]. `class` and `user_agent` come from closed
/// enums in [`crate::request_key`], carried as strings for the same forward
/// compatibility as [`StatusPayload::request_key_mode`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnkeyedCount {
    /// The `CONNECT` host class (`api.anthropic.com`, `other`, ...) or the
    /// request path class (`messages`, `api`, ...).
    pub class: String,
    /// The user-agent family (`claude-cli`, `curl`, `none`, ...).
    pub user_agent: String,
    pub count: u64,
}

/// What a running proxy's MITM listener can intercept, by its policy and loaded
/// leaf: [`StatusPayload::mitm`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MitmStatus {
    /// The hosts the listener can terminate, [`crate::mitm::ALLOWED_HOSTS`]
    /// intersected with the loaded leaf's coverage
    /// ([`crate::mitm::TlsAssets::intercepted_hosts`]). Empty when MITM is off.
    /// `chatgpt.com` here says the leaf allows it, not that it is terminated:
    /// the listener does that only while the pool holds a Codex account, counted
    /// on every CONNECT, and the payload's account rows say whether it does.
    #[serde(default)]
    pub intercepted_hosts: Vec<String>,
    /// Which leaf the listener presents: [`Self::LEAF_MINTED`] or
    /// [`Self::LEAF_NONE`]. A string rather than an
    /// enum because a closed enum here would make an OLDER client fail the
    /// whole payload parse on the day a value is added, and fall back to the
    /// offline snapshot; an unknown string costs only this one decision.
    #[serde(default)]
    pub leaf: String,
}

impl MitmStatus {
    /// A leaf tcr minted under `tcr-ca.pem` ([`crate::mitm::minted_ca_path`]).
    pub const LEAF_MINTED: &'static str = "minted";
    /// No TLS material loaded: the boot-time load failed, or this process
    /// never loaded any. MITM is off and every `CONNECT` is blind-tunnelled.
    pub const LEAF_NONE: &'static str = "none";

    /// The status of a listener that loaded `assets`, or of one that loaded
    /// nothing (`None`).
    pub fn of(assets: Option<&crate::mitm::TlsAssets>) -> Self {
        let Some(assets) = assets else {
            return Self {
                intercepted_hosts: Vec::new(),
                leaf: Self::LEAF_NONE.to_string(),
            };
        };
        Self {
            intercepted_hosts: assets.intercepted_hosts(),
            leaf: Self::LEAF_MINTED.to_string(),
        }
    }

    /// Does the listener's leaf and policy let it terminate `host`?
    /// Case-insensitive, as the listener's own check is. For `chatgpt.com` the
    /// pool must also hold a Codex account, which the account rows answer.
    pub fn intercepts(&self, host: &str) -> bool {
        self.intercepted_hosts
            .iter()
            .any(|h| h.eq_ignore_ascii_case(host))
    }
}

/// One account's live row. Field-for-field the serializable half of
/// [`AccountSnapshot`], plus the server's own `threshold` for that account.
///
/// The threshold rides along deliberately: it decides which windows read as
/// "held", and re-deriving it on the client from `~/.config/tcr.json`
/// would silently use a file that may have been edited since the server booted —
/// and, worse, would zip a client-ordered threshold list against a
/// server-ordered account list. The server is the authority on its own state.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountStatus {
    pub name: String,
    pub priority: i64,
    pub status: String,
    pub disabled: bool,
    pub five_hour: Option<f64>,
    /// Timestamps cross the wire as Unix milliseconds — the same unit the config's
    /// `expiresAt` and every internal deadline already use.
    pub five_hour_reset_ms: Option<i64>,
    /// [`AccountSnapshot::five_hour_absent_on_plan`]: the plan has no 5-hour window.
    /// On this server-to-CLI wire because only the server probed it and the live
    /// `tcr status` text renders it as `5h=none`. Skipped when `false`, and a
    /// missing key reads `false`, so both skew directions show `5h=n/a` exactly as
    /// before; no [`STATUS_KIND`] bump.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub five_hour_absent_on_plan: bool,
    pub seven_day: Option<f64>,
    pub seven_day_reset_ms: Option<i64>,
    pub seven_day_oi: Option<f64>,
    /// The Fable weekly window's reset, mirroring [`Self::seven_day_reset_ms`].
    /// `#[serde(default, skip_serializing_if = "Option::is_none")]` for the same
    /// forward/back-compat reason as `usage` below: an OLDER server's payload
    /// has no such key (reads as `None`, the truth — "this server does not
    /// report it"), and an OLDER client simply never looks at the extra key on
    /// a NEWER server's payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seven_day_oi_reset_ms: Option<i64>,
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub last_used_ms: Option<i64>,
    pub rate_limited_until_ms: Option<i64>,
    pub probe_status: ProbeStatus,
    pub last_probe_ms: Option<i64>,
    /// [`AccountSnapshot::quota_updated_at`]: when a quota reading was last taken,
    /// from a probe or response headers. `#[serde(default)]` so an older server's
    /// payload, which has no such key, decodes as `None` ("not reported"); no
    /// [`STATUS_KIND`] bump, for the reason [`StatusPayload::peers_error`] gives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_updated_at_ms: Option<i64>,
    pub probe_error: Option<String>,
    pub quota_state: QuotaState,
    pub gate: GateReason,
    pub free_at_ms: Option<i64>,
    /// The account's effective switch threshold on the SERVER (its own
    /// `switchThreshold`, else the global one).
    pub threshold: f64,
    /// Decayed count of stream failures — an in-band SSE `error` event, or a
    /// stream that hit EOF without Anthropic's `message_stop` terminator
    /// (recorded as `"truncated"`); see [`AccountSnapshot::stream_error_count`].
    /// Put ON the wire deliberately —
    /// it carries no credential material and `tcr status --json` is how an
    /// operator sees the fleet (`tcr status --json | jq '.[].streamErrorCount'`);
    /// see this module's doc comment on the no-secret invariant.
    ///
    /// `#[serde(default)]` is load-bearing, not tidiness. This field was ADDED,
    /// and a newer client routinely talks to an older still-running server: the
    /// binary on disk is rebuilt on merge while the live process keeps serving
    /// until someone restarts it, which is the normal state of this system, not
    /// an edge case. Without a default, that skew is a hard deserialize failure
    /// ("missing field `streamErrorCount`") and `tcr status` falls back to an
    /// OFFLINE snapshot whose serving counters are all structural zeros — the
    /// operator is shown a fabricated healthy fleet at exactly the moment the
    /// real one is on fire. Observed live 2026-08-04: client bd60839 against
    /// server 325df03 reported all 13 accounts `active` while the log carried
    /// 52 rate-limit events and 8 accounts sat on hour-long holds.
    ///
    /// Absent → 0, which reads identically to "no stream errors seen". That is
    /// the honest degradation: `source` on the same payload already tells the
    /// operator the reading came from a server that could not report it.
    #[serde(default)]
    pub stream_error_count: usize,
    /// The most recent stream error's type, alongside the count above. Same
    /// on-the-wire decision as `stream_error_count`; rendered as `lastStreamError`.
    pub last_stream_error: Option<String>,
    /// Group labels for this account, mirroring [`AccountSnapshot::groups`].
    /// `#[serde(default)]` for the same forward-compat reason as
    /// `stream_error_count`: an older server that predates groups omits the
    /// field, and a newer client must still deserialize its payload rather than
    /// falling back to a fabricated offline snapshot.
    #[serde(default)]
    pub groups: Vec<String>,
    /// The reserved subset of [`Self::groups`], mirroring
    /// [`AccountSnapshot::reserved_groups`]. `#[serde(default)]` for the same
    /// forward-compat reason as `groups` — an older server predates
    /// reservation entirely.
    #[serde(default)]
    pub reserved_groups: Vec<String>,
    /// The parked subset of [`Self::groups`], mirroring
    /// [`AccountSnapshot::parked_groups`]. `#[serde(default)]` for the same
    /// forward-compat reason as `reserved_groups` above — an older server
    /// predates parking entirely.
    #[serde(default)]
    pub parked_groups: Vec<String>,
    /// The opted-in subset of [`Self::groups`] — see
    /// [`AccountSnapshot::control_allowed_groups`]. `#[serde(default)]` for the
    /// same forward-compat reason as the two fields above: a server that
    /// predates the opt-in omits it entirely.
    #[serde(default)]
    pub control_allowed_groups: Vec<String>,
    /// Which upstream this account belongs to — see [`AccountSnapshot::provider`].
    /// On this wire for the same reason as the plan fields below: only the server
    /// has it, and the CLI needs it to name a Codex row's plan and to put
    /// `provider` on `tcr status --json`.
    ///
    /// Skipped when Anthropic, so an Anthropic-only fleet's payload is exactly
    /// what it was before the field existed, and an absent key reads as
    /// Anthropic, which is how an older server treats every account. Neither
    /// skew direction is MISREAD, so, per [`StatusPayload::build`]'s doc-comment,
    /// this must NOT bump [`STATUS_KIND`].
    ///
    /// The enum is closed on this wire, though, as `GateReason`, `ProbeStatus`
    /// and `QuotaState` are: a variant added later makes an OLDER CLI fail the
    /// whole payload parse and fall back to the offline snapshot (the hazard
    /// [`StatusPayload::build`]'s doc-comment describes), so a new provider needs
    /// a plan for older clients, not just a new arm.
    #[serde(
        default,
        skip_serializing_if = "crate::provider::Provider::is_anthropic"
    )]
    pub provider: crate::provider::Provider,
    /// The account's plan as the profile endpoint reported it, verbatim, and the
    /// two fields that refine it — mirroring [`AccountSnapshot`]'s three.
    ///
    /// On this wire (server → CLI) rather than derived at either end, because
    /// only the server has them: they are read from the config the SERVING
    /// process loaded, and the CLI's live path has no config of its own to
    /// consult. The customer-facing label is still derived once, one layer out,
    /// by `render_accounts`/`render_accounts_json`.
    ///
    /// `#[serde(default)]` for the same forward-compat reason as `groups` above,
    /// and it degrades honestly in both directions: an older server omits the
    /// keys, a newer client reads `None`, and every surface renders "plan
    /// unknown" — which is the truth about a server that cannot report one.
    #[serde(default)]
    pub organization_type: Option<String>,
    #[serde(default)]
    pub rate_limit_tier: Option<String>,
    #[serde(default)]
    pub seat_tier: Option<String>,
    #[serde(default)]
    pub plan_display_name: Option<String>,
    /// The org this account is scoped to — see [`AccountSnapshot::org_uuid`]:
    /// a fact for a client to show, never an address. `#[serde(default)]`
    /// for the same forward-compat reason as the fields above.
    #[serde(default)]
    pub org_uuid: Option<String>,
    #[serde(default)]
    pub org_name: Option<String>,
    /// Proxy-computed usage and cost — see [`AccountSnapshot::usage`].
    ///
    /// Both skew directions degrade HONESTLY, which is exactly what
    /// [`StatusPayload::build`]'s doc-comment reserves a [`STATUS_KIND`] bump
    /// for NOT doing — so this must not bump it:
    ///
    /// * OLD client ← NEW server: serde skips the unknown key; every field the
    ///   old client reads is untouched.
    /// * NEW client ← OLD server: the key is absent and reads as `None`, and
    ///   the client renders "this server does not report usage" — the truth.
    ///
    /// **The `Option` is what carries that second direction, not the
    /// `#[serde(default)]`.** serde's `missing_field` hands the field a
    /// `MissingFieldDeserializer` whose `deserialize_option` returns
    /// `visit_none` (serde 1.0.228, `src/private/de.rs:45-50`), so an absent
    /// `Option<T>` is `None` with or without the attribute. Verified by
    /// mutation: removing `default` here leaves every test green.
    ///
    /// That is NOT true of a non-`Option` field, which is where the real hazard
    /// lives — see [`Self::stream_error_count`], a bare `usize` whose `default`
    /// genuinely is load-bearing and whose absence took down a whole fleet view
    /// on 2026-08-04. `default` is kept here for consistency with the optional
    /// fields beside it; `skip_serializing_if` is doing real work, keeping the
    /// key off the wire entirely rather than emitting `"usage": null`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<tcr_status_wire::UsageRow>,
    // The facts below are `AccountSnapshot::codex`, flattened onto this row
    // under the names `tcr status --json` gives them, with the same attributes as
    // on `tcr_status_wire::AccountStatusRow`. Each is skipped when it holds
    // nothing, which is always on an Anthropic account, so an Anthropic-only
    // fleet's payload is the bytes it was before they existed, and a payload from
    // an OLD server reads every one as "not reported". Neither skew direction is
    // MISREAD, so, per `StatusPayload::build`'s doc-comment, none bumps
    // `STATUS_KIND`. See `CodexFacts` for what each one means.
    /// [`CodexFacts::quota_windows`]: `Some(vec![])` is a plan that reported no
    /// window, and stays apart from `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_windows: Option<Vec<String>>,
    /// [`CodexFacts::five_hour_window_minutes`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub five_hour_window_minutes: Option<u32>,
    /// [`CodexFacts::seven_day_window_minutes`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seven_day_window_minutes: Option<u32>,
    /// [`CodexFacts::limit_reached_type`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_reached_type: Option<String>,
    /// [`CodexFacts::spend_control_reached`]. A bare `bool`, so its `default`
    /// is load-bearing for the reason [`Self::stream_error_count`] gives.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub spend_control_reached: bool,
    /// [`CodexFacts::credits`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credits: Option<tcr_status_wire::CodexCredits>,
    /// Banked reset summary, without any credit identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_credits: Option<tcr_status_wire::ResetCredits>,
    /// [`CodexFacts::banked_resets`]: a Claude row's banked limit resets, without grant ids.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub banked_resets: Option<tcr_status_wire::BankedResets>,
    /// [`CodexFacts::auto_reset`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_reset: Option<tcr_status_wire::AutoReset>,
    /// [`CodexFacts::login_error`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login_error: Option<String>,
    /// [`CodexFacts::codex_home`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_home: Option<String>,
    /// Refreshed tokens still need guarded publication to the imported Codex login.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub token_sync_pending: bool,
}

/// Unix milliseconds for an instant, matching [`crate::now_ms`]'s unit.
const fn to_ms(t: OffsetDateTime) -> i64 {
    (t.unix_timestamp_nanos() / 1_000_000) as i64
}

/// Inverse of [`to_ms`]. `None` on an out-of-range value rather than a panic — a
/// nonsense timestamp from the wire degrades one rendered field, never the run.
fn from_ms(ms: i64) -> Option<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000).ok()
}

impl StatusPayload {
    /// Does the serving proxy present a leaf tcr minted that lets it terminate
    /// `host`? `false` when the payload reports no MITM status at all (an older
    /// server), or another leaf. For `chatgpt.com` the pool must also hold a
    /// Codex account, which the account rows answer.
    pub fn minted_leaf_intercepts(&self, host: &str) -> bool {
        self.mitm
            .as_ref()
            .is_some_and(|mitm| mitm.leaf == MitmStatus::LEAF_MINTED && mitm.intercepts(host))
    }

    /// What the serving MITM listener can terminate ([`Self::mitm`]) and the sha of the build
    /// the server runs, the two facts a Codex route decision names. `Err` with that sha from a
    /// server older than the `mitm` key: it predates Codex interception, and the route it needs
    /// is a restart on a current build.
    pub fn codex_interception(&self) -> Result<(&MitmStatus, &str), &str> {
        let sha = self.build.sha.as_str();
        self.mitm.as_ref().map(|mitm| (mitm, sha)).ok_or(sha)
    }

    /// The account rows of `provider` this payload lists, in any state.
    pub fn accounts_of(
        &self,
        provider: crate::provider::Provider,
    ) -> impl Iterator<Item = &AccountStatus> {
        self.accounts
            .iter()
            .filter(move |row| row.provider == provider)
    }

    /// Does this payload list an account of `provider`, in any state?
    pub fn holds_provider(&self, provider: crate::provider::Provider) -> bool {
        self.accounts_of(provider).next().is_some()
    }

    /// Does this payload list an account named `name`, in any state (a disabled row too)?
    pub fn lists_account(&self, name: &str) -> bool {
        self.accounts.iter().any(|row| row.name == name)
    }

    /// [`Self::accounts_of`], or `None` from a server older than the [`Self::mitm`] key. Such a
    /// server predates Codex pooling: its rows carry no `provider`, so every one reads as
    /// Anthropic, and the caller answers from its config instead.
    pub fn current_accounts_of(
        &self,
        provider: crate::provider::Provider,
    ) -> Option<impl Iterator<Item = &AccountStatus>> {
        self.mitm.as_ref().map(|_| self.accounts_of(provider))
    }

    /// Requests the serving proxy answered for `provider`'s sessions in the last `minutes`
    /// wall-clock minutes, summed over each session's per-minute sparkline
    /// ([`tcr_status_wire::SessionRow::req_per_minute`], 30 entries, oldest first).
    ///
    /// The sparkline rather than [`AccountStatus::requests`]: that counter is cumulative since
    /// the server booted, so it answers "has this proxy ever served anything", and a route that
    /// broke an hour ago would still read as healthy.
    ///
    /// One provider at a time, because a Codex session's traffic says nothing about Claude's
    /// route, and counting it would read a broken Claude route as healthy. A session names its
    /// provider only when it is not Anthropic ([`tcr_status_wire::SessionRow::provider`]).
    pub fn requests_in_window(&self, provider: crate::provider::Provider, minutes: usize) -> u64 {
        let wire = match provider {
            crate::provider::Provider::Anthropic => None,
            crate::provider::Provider::Codex => Some(provider.as_str()),
        };
        self.sessions
            .iter()
            .filter(|session| session.provider.as_deref() == wire)
            .map(|session| session_requests_in_window(session, minutes))
            .sum()
    }

    /// Does the serving process understand request keys? Its [`Self::request_key_mode`] is
    /// sent whenever it holds one, and that PRESENCE is the contract a launcher reads before
    /// it adds the key to a launch. `false` from a server that holds no key or predates them.
    pub const fn understands_request_keys(&self) -> bool {
        self.request_key_mode.is_some()
    }

    /// The server's heat reading ([`Self::heat`]), or why there is none.
    pub fn heat_reading(&self) -> Result<&tcr_status_wire::HeatRow, Unread<'_>> {
        Lenient::read(self.heat.as_ref())
    }

    /// The server's fan-control reading ([`Self::fan_control`]), or why there is none.
    pub fn fan_control_reading(&self) -> Result<&tcr_status_wire::FanControlRow, Unread<'_>> {
        Lenient::read(self.fan_control.as_ref())
    }

    /// Project a live snapshot onto the wire. `thresholds` is the server's own
    /// per-account list (see [`crate::manager::Manager::thresholds`]); a short
    /// list falls back to `1.0` per account, which can only fail CLOSED — at 1.0
    /// nothing but a fully-exhausted window reads as held, never a false hold.
    pub fn from_snapshot(
        snapshot: &StatsSnapshot,
        thresholds: &[f64],
        http1_only: bool,
        control: Option<String>,
        group_colors: std::collections::BTreeMap<String, String>,
    ) -> Self {
        let accounts = snapshot
            .accounts
            .iter()
            .enumerate()
            .map(|(i, a)| AccountStatus {
                name: a.name.clone(),
                priority: a.priority,
                status: a.status.clone(),
                disabled: a.disabled,
                five_hour: a.five_hour,
                five_hour_reset_ms: a.five_hour_reset.map(to_ms),
                five_hour_absent_on_plan: a.five_hour_absent_on_plan,
                seven_day: a.seven_day,
                seven_day_reset_ms: a.seven_day_reset.map(to_ms),
                seven_day_oi: a.seven_day_oi,
                seven_day_oi_reset_ms: a.seven_day_oi_reset.map(to_ms),
                requests: a.requests,
                input_tokens: a.input_tokens,
                output_tokens: a.output_tokens,
                cache_read_tokens: a.cache_read_tokens,
                cache_creation_tokens: a.cache_creation_tokens,
                last_used_ms: a.last_used.map(to_ms),
                rate_limited_until_ms: a.rate_limited_until.map(to_ms),
                probe_status: a.probe_status,
                last_probe_ms: a.last_probe.map(to_ms),
                quota_updated_at_ms: a.quota_updated_at.map(to_ms),
                probe_error: a.probe_error.clone(),
                quota_state: a.quota_state,
                gate: a.gate,
                free_at_ms: a.free_at.map(to_ms),
                threshold: thresholds.get(i).copied().unwrap_or(1.0),
                stream_error_count: a.stream_error_count,
                last_stream_error: a.last_stream_error.clone(),
                groups: a.groups.clone(),
                reserved_groups: a.reserved_groups.clone(),
                parked_groups: a.parked_groups.clone(),
                control_allowed_groups: a.control_allowed_groups.clone(),
                provider: a.provider,
                organization_type: a.organization_type.clone(),
                rate_limit_tier: a.rate_limit_tier.clone(),
                seat_tier: a.seat_tier.clone(),
                plan_display_name: a.plan_display_name.clone(),
                org_uuid: a.org_uuid.clone(),
                org_name: a.org_name.clone(),
                usage: a.usage.clone(),
                quota_windows: a.codex.quota_windows.clone(),
                five_hour_window_minutes: a.codex.five_hour_window_minutes,
                seven_day_window_minutes: a.codex.seven_day_window_minutes,
                limit_reached_type: a.codex.limit_reached_type.clone(),
                spend_control_reached: a.codex.spend_control_reached,
                credits: a.codex.credits.clone(),
                reset_credits: a.codex.reset_credits.clone(),
                banked_resets: a.codex.banked_resets.clone(),
                auto_reset: a.codex.auto_reset.clone(),
                login_error: a.codex.login_error.clone(),
                codex_home: a.codex.codex_home.clone(),
                token_sync_pending: a.codex.token_sync_pending,
            })
            .collect();
        Self {
            kind: STATUS_KIND.to_string(),
            accounts,
            // Compile-time constants of the SERVING binary — not passed in,
            // because the only honest answer is the one baked into this process.
            build: BuildInfo::current(),
            http1_only,
            control,
            group_colors,
            sessions: snapshot.wire_sessions.clone(),
            sessions_summary: snapshot.wire_sessions_summary.clone(),
            // EMPTY here, and filled by the caller that holds the peers file,
            // the peer state file and the lease ledger, none of which is in a
            // [`StatsSnapshot`], which is the accounts view and nothing else.
            // Deriving them here would mean this function reading two files off
            // disk on every status request, and it takes no paths. The serving
            // process assigns [`Self::peers`] from [`peer_status_rows`] after
            // this call; an empty list is the honest answer for every other
            // caller (the CLI's own round-trip tests, `into_snapshot`).
            peers: Vec::new(),
            peers_error: None,
            // `None` here for the same reason as `peers`: what the listener's policy
            // and leaf allow is boot state of the serving process, not a fact in a
            // `StatsSnapshot`. The serving process assigns `Some`.
            mitm: None,
            // `None` for the same reason again. The Codex traffic counts live in
            // the serving process's own rings, and the upstream is its boot
            // config; a `StatsSnapshot` carries neither, so the serving process
            // assigns both after this call.
            codex_traffic: None,
            codex_upstream: None,
            // The request key is the serving process's boot state too.
            request_key_mode: None,
            unkeyed_connects: None,
            unkeyed_requests: None,
            // The machine's heat is read by the serving process, off its runtime.
            heat: None,
            // So is the fan controller, which only the serving process runs.
            fan_control: None,
        }
    }

    /// Rebuild the snapshot the CLI renderers take, plus the server's thresholds.
    ///
    /// The `recent` log and the affinity-hash `sessions` table (`StatsSnapshot::sessions`,
    /// the TUI pin display) come back EMPTY and `current` `None`: they are not on the wire
    /// (see the module docs) and no `tcr status` renderer reads them.
    /// `StatsSnapshot::wire_sessions`, by contrast, IS reconstructed from
    /// [`Self::sessions`] — it genuinely round-trips.
    /// Every `AccountSnapshot` field is reconstructed too — the struct literal below names
    /// all of them, so a new field forces an explicit decision here instead of silently
    /// rendering a default.
    pub fn into_snapshot(self) -> (StatsSnapshot, Vec<f64>) {
        let mut thresholds = Vec::with_capacity(self.accounts.len());
        let accounts = self
            .accounts
            .into_iter()
            .enumerate()
            .map(|(position, a)| {
                thresholds.push(a.threshold);
                AccountSnapshot {
                    // The row's position in the document. The wire carries no slot, nothing
                    // that renders a rebuilt snapshot reads one, and no key acts on a row of
                    // it, so its position is the only honest value.
                    slot: position,
                    name: a.name,
                    provider: a.provider,
                    organization_type: a.organization_type,
                    rate_limit_tier: a.rate_limit_tier,
                    seat_tier: a.seat_tier,
                    plan_display_name: a.plan_display_name,
                    org_uuid: a.org_uuid,
                    org_name: a.org_name,
                    priority: a.priority,
                    status: a.status,
                    disabled: a.disabled,
                    five_hour: a.five_hour,
                    five_hour_reset: a.five_hour_reset_ms.and_then(from_ms),
                    five_hour_absent_on_plan: a.five_hour_absent_on_plan,
                    seven_day: a.seven_day,
                    seven_day_reset: a.seven_day_reset_ms.and_then(from_ms),
                    seven_day_oi: a.seven_day_oi,
                    seven_day_oi_reset: a.seven_day_oi_reset_ms.and_then(from_ms),
                    requests: a.requests,
                    input_tokens: a.input_tokens,
                    output_tokens: a.output_tokens,
                    cache_read_tokens: a.cache_read_tokens,
                    cache_creation_tokens: a.cache_creation_tokens,
                    last_used: a.last_used_ms.and_then(from_ms),
                    rate_limited_until: a.rate_limited_until_ms.and_then(from_ms),
                    probe_status: a.probe_status,
                    last_probe: a.last_probe_ms.and_then(from_ms),
                    quota_updated_at: a.quota_updated_at_ms.and_then(from_ms),
                    probe_error: a.probe_error,
                    quota_state: a.quota_state,
                    gate: a.gate,
                    free_at: a.free_at_ms.and_then(from_ms),
                    stream_error_count: a.stream_error_count,
                    last_stream_error: a.last_stream_error,
                    groups: a.groups,
                    reserved_groups: a.reserved_groups,
                    parked_groups: a.parked_groups,
                    control_allowed_groups: a.control_allowed_groups,
                    usage: a.usage,
                    codex: CodexFacts {
                        quota_windows: a.quota_windows,
                        five_hour_window_minutes: a.five_hour_window_minutes,
                        seven_day_window_minutes: a.seven_day_window_minutes,
                        limit_reached_type: a.limit_reached_type,
                        spend_control_reached: a.spend_control_reached,
                        credits: a.credits,
                        reset_credits: a.reset_credits,
                        banked_resets: a.banked_resets,
                        auto_reset: a.auto_reset,
                        login_error: a.login_error,
                        codex_home: a.codex_home,
                        token_sync_pending: a.token_sync_pending,
                    },
                }
            })
            .collect();
        (
            StatsSnapshot {
                accounts,
                current: None,
                current_by_provider: crate::stats::CurrentByProvider::default(),
                recent: Vec::new(),
                sessions: Vec::new(),
                wire_sessions: self.sessions,
                wire_sessions_summary: self.sessions_summary,
            },
            thresholds,
        )
    }
}

/// Discriminator carried by every quota response, for the same reason as
/// [`STATUS_KIND`]: a tcr too old to have the quota route answers it with a local
/// 404, and a caller should trust a body only when this matches.
pub const QUOTA_KIND: &str = "tcr.quota.v1";

/// The light quota read served on [`crate::proxy::QUOTA_PATH`], for a status line
/// that ticks every turn and has no use for the rest of [`StatusPayload`].
///
/// Built from the same [`StatsSnapshot`] as the status payload, so the gate and
/// the utilizations are the ones selection and `tcr status` see. Nothing here is
/// fetched: no probe and no upstream call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaPayload {
    /// Always [`QUOTA_KIND`].
    pub kind: String,
    pub accounts: Vec<AccountQuota>,
}

/// One account's row on the quota route: why it is out of rotation, if it is,
/// and its 5-hour and weekly utilization with their resets. Field names and
/// units match [`AccountStatus`], so a client reading both decodes one way.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountQuota {
    pub name: String,
    /// The shared upstream discriminator. The v1 quota projection includes only
    /// Anthropic accounts and omits their default provider field, preserving the
    /// documented Claude-only wire shape.
    #[serde(
        default,
        skip_serializing_if = "crate::provider::Provider::is_anthropic"
    )]
    pub provider: crate::provider::Provider,
    pub gate: GateReason,
    pub five_hour: Option<f64>,
    pub five_hour_reset_ms: Option<i64>,
    pub seven_day: Option<f64>,
    pub seven_day_reset_ms: Option<i64>,
}

impl QuotaPayload {
    /// Project Claude capacity onto the documented v1 quota wire, in server order.
    /// Codex capacity belongs to the provider-aware status route, not this v1 shape.
    pub fn from_snapshot(snapshot: &StatsSnapshot) -> Self {
        let accounts = snapshot
            .accounts
            .iter()
            .filter(|account| match account.provider {
                crate::provider::Provider::Anthropic => true,
                crate::provider::Provider::Codex => false,
            })
            .map(|a| AccountQuota {
                name: a.name.clone(),
                provider: a.provider,
                gate: a.gate,
                five_hour: a.five_hour,
                five_hour_reset_ms: a.five_hour_reset.map(to_ms),
                seven_day: a.seven_day,
                seven_day_reset_ms: a.seven_day_reset.map(to_ms),
            })
            .collect();
        Self {
            kind: QUOTA_KIND.to_string(),
            accounts,
        }
    }
}

pub use peer_graph::{
    GraphEdge, GraphEdgeDetail, GraphNode, GraphRole, PEER_GRAPH_KIND, PeerGraph, peer_graph,
    peer_graph_block,
};
pub use peer_ls::{PeerCapsJson, PeerExitJson, PeerFoundRow, PeerLsJson, PeerLsPeer, PeerLsRow};
pub use peer_rows::{
    PathKind, PathStatus, PeerStatusRow, PeerTailnetStatus, masked_label, peer_status_rows,
    peers_block, peers_block_with_error,
};

#[cfg(test)]
mod tests;
