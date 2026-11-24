//! The config file, `~/.config/tcr.json`.
//!
//! The structs use a camelCase shape and stay tolerant of fields we do not
//! model (`routes`, `sx`, `quotaProbeSeconds`, `warmupSeconds`, …). Every
//! struct carries a flattened `extra` map so an unknown key survives a
//! load→save round-trip untouched.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

mod account_edit;
mod color;
mod credential_version;
mod edit;
mod file;
mod group_edit;
mod load;
mod tokens;

pub use account_edit::{
    AccountRemoval, AccountWrite, CLAUDE_CODE_IMPORT_KEY, remove_account_entry, save_account,
};
pub use color::{ColorSource, best_readable_apca, derive_group_color, validate_hex_color};
pub(crate) use edit::{CODEX_SYNC_PENDING_KEY, save_codex_token_sync_pending};
pub use edit::{
    ControlWrite, DisabledWrite, DocEdit, Edit, GroupWrite, PlanWrite, RequestKeyWrite,
    read_request_key, save_control_account, save_disabled, save_group_membership, save_plan,
    save_request_key,
};
pub use file::save;
pub(crate) use file::{Durability, read_document, write_atomic, write_atomic_with};
pub use group_edit::{
    GroupColorWrite, GroupControlWrite, GroupParkWrite, GroupReserveWrite,
    save_group_allow_control, save_group_color, save_group_parked, save_group_reserved,
};
pub use load::{load, load_or_init};
pub use tokens::{CredentialScope, save_settings, save_tokens, save_tokens_for};

/// Typed config-layer errors: I/O vs malformed JSON stay distinguishable so a
/// caller can tell "no config yet" from "config is corrupt".
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("config parse error: {0}")]
    Parse(#[from] serde_json::Error),
}

const fn default_port() -> u16 {
    3456
}

/// `pub(crate)` so `peer::lease` can read the one spelling of this address
/// rather than keeping a second copy of the literal.
pub(crate) fn default_upstream() -> String {
    "https://api.anthropic.com".to_string()
}

/// Default [`Config::codex_upstream`].
pub(crate) fn default_codex_upstream() -> String {
    crate::provider::CODEX_UPSTREAM.to_string()
}

/// The shared default-omission rule for config serialization, status and Codex routing.
fn is_default_codex_upstream(value: &str) -> bool {
    value == crate::provider::CODEX_UPSTREAM
}

/// Name an upstream override consistently on every provider-aware status surface.
pub(crate) fn non_default_codex_upstream(value: &str) -> Option<String> {
    (!is_default_codex_upstream(value)).then(|| value.to_owned())
}

const fn default_switch_threshold() -> f64 {
    0.95
}

/// Default headroom (§3, control-account part 2) reserved for the control
/// account against general (non-control-preferred) picks — see
/// [`Config::control_reserve`] and [`crate::manager::select::effective_threshold`].
const fn default_control_reserve() -> f64 {
    0.05
}

/// Default [`Config::reset_urgency_tier_hours`]: 24h buckets.
///
/// Rotation ranks an eligible account by which 24-hour bucket its governing
/// weekly reset falls into, ahead of the LRU tick — so a fleet spends the
/// account whose unused weekly headroom expires soonest, before the one whose
/// window has days left to run. Unused weekly quota is worth nothing after its
/// reset, and on the live fleet (measured 2026-08-26, 13 accounts, all at
/// priority 0) the resets were spread from 2.5h to 152.5h out, with the
/// soonest-resetting account sitting on 44% unspent.
///
/// The bucket width is what keeps this from becoming the deterministic pin that
/// [`crate::manager::Manager::select`] already rejected headroom-ordering for. A
/// RAW reset instant is millisecond-precision and never ties, so ranking on it
/// directly would retire the LRU tick entirely and park every unpinned request
/// on one account until it gated. Bucketing restores the tie: at 24h the live
/// fleet's four soonest accounts share one bucket and still fan out inside it.
///
/// Wider buckets weaken the urgency signal (at 48h, six of the thirteen collapse
/// into one bucket); narrower ones concentrate load (at 6h the soonest account
/// is alone in its bucket, which is the raw behaviour with extra steps). `0`
/// disables the tier term outright and restores the pre-tier ordering — the
/// config-only rollback, no rebuild.
const fn default_reset_urgency_tier_hours() -> u32 {
    24
}

/// Default [`Config::usage_retention_days`]: a quarter of history, which is ~180 MB
/// at the observed ~2 MB/day and answers "how did last month compare".
const fn default_usage_retention_days() -> u32 {
    90
}

/// Default pacing when the `pacing` key is absent: OFF — no in-flight cap, no
/// min-spacing, so an unconfigured proxy runs the no-pacing selection path.
///
/// A per-account concurrency cap trades prompt-cache locality for load spread:
/// every request it diverts lands on an account whose prefix is cold. On a
/// single-user proxy the cache is the scarce resource and the accounts are not,
/// so the trade is the wrong way round and the cap ships off. It stays a
/// supported knob — set `"pacing": {"maxInFlightPerAccount": N}` (and/or
/// `"minSpacingMs"`) to turn it back on, with exactly the behaviour it has today.
///
/// This is NOT covered by the egress throttles ([`default_account_throttle`] /
/// [`default_fleet_throttle`], `src/manager/throttle.rs`): those are RATE limiters
/// (min-spacing + burst at the send site), not concurrency bounds, and they are
/// deliberately not a substitute for one. Turning the cap off leaves per-account
/// concurrency genuinely unbounded.
const fn default_pacing() -> PacingConfig {
    PacingConfig {
        max_in_flight_per_account: None,
        min_spacing_ms: None,
    }
}

/// Default PER-ORGANIZATION outbound throttle: ON. Absent `accountThrottle` key →
/// these values; `"accountThrottle": {}` → off (escape hatch).
///
/// **The burst is sized for an agent wave, not for one turn.** The sustained rate
/// is unchanged: one send per 350ms per organization. `burst` went 4 → 8 so that
/// one Claude Code turn (inference + `count_tokens` + a telemetry batch, roughly
/// 4-6 requests) passes untaxed, then 8 → 24 on 2026-10-12 because 8 still taxed
/// fan-out: a wave of subagents pinned to one account, or a batch of new sessions
/// whose startup calls (`/api/claude_code/settings`, `policy_limits`) all go to the
/// control account. Replaying 575k logged sends (five days, 18 accounts) through
/// this GCRA: at `burst: 8`, 0.4% of `/v1/messages` sends waited, the worst a
/// 25-wide wave on one account whose last send waited at least 4.1s. At
/// `burst: 24`, with the fleet ceiling at 32, none did. The replay works from
/// completion timestamps, which are smoother than arrivals, so its waits are
/// lower bounds.
///
/// **Why the looser burst is not expected to draw 429s.** Over the same five days
/// Anthropic returned 55 "would exceed your account's rate limit" 429s, all on
/// `/v1/messages`. In the 60s before each one, that account had sent a median of
/// 39 requests, against the ~179 a minute this bucket allows; and one account ran
/// at the full 350ms rate for a minute (187 sends) with no 429. Whatever trips
/// those 429s, it is not request count.
///
/// `tests/throttle_exempt.rs::shipped_tuning_passes_an_agent_wave_untaxed` holds
/// the 24, and `shipped_tuning_still_paces_beyond_one_wave` proves the bucket
/// still paces past it. Both read this function's values, not a copy.
///
/// The speedup for a burst SPREAD across accounts does not come from this. It
/// comes from the bucket being per-org rather than shared, so capacity grows with
/// the account pool; a spread burst is free at any of these sizes.
const fn default_account_throttle() -> ThrottleConfig {
    ThrottleConfig {
        min_spacing_ms: Some(350),
        burst: Some(24),
    }
}

/// Default ceiling for each provider's fleet: ON, and deliberately far looser than
/// [`default_account_throttle`].
///
/// This is INSURANCE against a mechanism nobody here has measured: that something
/// SHARED across one provider's accounts (the source IP, the `client_id`) is rate-limited in
/// addition to the per-organization limits. Whether such a limiter exists is
/// genuinely unknown — see [`ThrottleConfig`]'s note on what the evidence does and
/// does not show.
///
/// **It is tuned so it can actually fire.** Insurance priced never to pay out is
/// not insurance, so the burst sits near the fleet's real peaks rather than far
/// above them. It shipped at 16 and went to 32 on 2026-10-12 for a structural
/// reason: the ceiling has to sit ABOVE the per-org burst
/// ([`default_account_throttle`], 24), or a one-account wave that the per-org
/// bucket admits gets held here instead. In the replay described there, per-org 24
/// under a ceiling of 16 still held 92 inference sends. At 32 the ceiling still
/// fires on real traffic: the same replay held 4 sends in five days, and the
/// fleet's busiest 1s and 2.8s windows reached 36 and 49 sends. Sustained
/// headroom is unchanged and large: the fleet averaged 1.5 req/s over those five
/// days, against this ceiling's 10 req/s.
///
/// `tests/throttle_exempt.rs::shipped_fleet_ceiling_can_actually_fire` is the
/// guard: it drives a 48-wide runaway and asserts the ceiling engages. A ceiling
/// never observed firing is indistinguishable from an absent one.
const fn default_fleet_throttle() -> ThrottleConfig {
    ThrottleConfig {
        min_spacing_ms: Some(100),
        burst: Some(32),
    }
}

/// What kind of credential an account row holds: its `type` key, parsed once
/// here at the serde boundary. No other code spells the strings.
///
/// An absent `type` is [`AccountType::OAuth`], as it always was. A string
/// tcr does not know is kept VERBATIM in [`AccountType::Unrecognized`]: the
/// row loads exactly as it did when `type` was a plain `String` (served with
/// its `accessToken`, never refreshed, probed or warmed, because only OAuth
/// rows are), and a save writes the same string back, so a value another tool
/// wrote survives tcr touching the file. A `type` that is not a string at all
/// is quarantined at load ([`Unusable::UnreadableType`]) rather than failing
/// the whole file.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(from = "AccountTypeWire", into = "AccountTypeWire")]
pub enum AccountType {
    /// A Claude subscription login: an OAuth access token, usually with a
    /// refresh token.
    #[default]
    OAuth,
    /// A console API key. tcr does not serve these: a file row of this type is
    /// quarantined at load ([`Unusable::ApiKeyAccount`]) and the live-add
    /// route refuses one.
    ApiKey,
    /// Any other string, held as written.
    Unrecognized(String),
}

impl AccountType {
    /// Only OAuth rows are refreshed, probed and warmed.
    pub const fn is_oauth(&self) -> bool {
        matches!(self, Self::OAuth)
    }
}

/// The on-disk spelling of [`AccountType`]: a known name, or any other string.
#[derive(Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum AccountTypeWire {
    Known(KnownAccountType),
    Unrecognized(String),
}

#[derive(Clone, Copy, Serialize, Deserialize)]
enum KnownAccountType {
    #[serde(rename = "oauth")]
    OAuth,
    #[serde(rename = "apikey")]
    ApiKey,
}

impl From<AccountTypeWire> for AccountType {
    fn from(wire: AccountTypeWire) -> Self {
        match wire {
            AccountTypeWire::Known(KnownAccountType::OAuth) => Self::OAuth,
            AccountTypeWire::Known(KnownAccountType::ApiKey) => Self::ApiKey,
            AccountTypeWire::Unrecognized(raw) => Self::Unrecognized(raw),
        }
    }
}

impl From<AccountType> for AccountTypeWire {
    fn from(kind: AccountType) -> Self {
        match kind {
            AccountType::OAuth => Self::Known(KnownAccountType::OAuth),
            AccountType::ApiKey => Self::Known(KnownAccountType::ApiKey),
            AccountType::Unrecognized(raw) => Self::Unrecognized(raw),
        }
    }
}

/// Proxy-level settings (`proxy` object in the file).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyConfig {
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// The request key ([`crate::request_key`]): what a caller presents to say
    /// it was launched on purpose. Separate from [`Self::api_key`], which gates
    /// the `/_tcr/` control routes. The server writes one at boot when the file
    /// has none ([`save_request_key`]); absent, behaviour is exactly as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_key: Option<String>,
    /// Any `proxy.*` keys we do not model, preserved verbatim on save.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            port: default_port(),
            api_key: None,
            request_key: None,
            extra: Map::new(),
        }
    }
}

/// One rotatable upstream account.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub name: String,
    #[serde(rename = "type", default)]
    pub account_type: AccountType,
    /// Which upstream this credential belongs to. Absent means Anthropic, which is what every
    /// account was before this key existed. Skipped on save when Anthropic for the same reason
    /// `egress` is skipped when local: a save rewrites every row, and an upgrade's first save
    /// must not stamp `"provider": "anthropic"` onto a file a human reads and edits.
    #[serde(
        default,
        skip_serializing_if = "crate::provider::Provider::is_anthropic"
    )]
    pub provider: crate::provider::Provider,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_uuid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_uuid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_name: Option<String>,
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Epoch **milliseconds** at which `access_token` expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub switch_threshold: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
    /// Labels used to prefer-route a session (`tcr run --group <name>`) to this
    /// account. Absent or empty means the account belongs to no group. Backward
    /// compatible in both directions: `Account` has no `deny_unknown_fields` and
    /// a flattened `extra` map, so a config already carrying `groups` round-trips
    /// today even before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub groups: Option<Vec<String>>,
    /// The organization's plan, VERBATIM as the profile endpoint reported it
    /// (`organization.organization_type`): `claude_max`, `claude_team`,
    /// `claude_pro`, and by the same naming pattern `claude_enterprise`.
    ///
    /// The raw provider string is what lands on disk, deliberately — the file
    /// is the honest record of what upstream said, and the customer-facing
    /// label is DERIVED from it by [`tcr_status_wire::plan_label`], in exactly
    /// one place. An unrecognized value therefore survives a round-trip and
    /// shows verbatim rather than being mis-bucketed into a plan we happen to
    /// know the name of.
    ///
    /// NOT `account.has_claude_max`, which is a different fact and a tempting
    /// wrong one: measured on a live fleet, it reads `true` on Team rows too,
    /// so it cannot tell a Max org from a Team org.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization_type: Option<String>,
    /// The organization's rate-limit tier, verbatim
    /// (`organization.rate_limit_tier`) — what distinguishes Max 20x
    /// (`default_claude_max_20x`) from Max 5x. Non-Max orgs carry their own
    /// values (`default_raven` observed on a Team org), which is why the tier
    /// alone is never read as the plan; see [`Self::organization_type`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_tier: Option<String>,
    /// The organization's seat tier, verbatim (`organization.seat_tier`) —
    /// which seat a seated org's row holds (`team_standard` and `team_tier_1`
    /// both observed on one live Team org). `None` on Max and Pro rows, which
    /// have no seats at all, which is why it refines only the seated plans in
    /// [`tcr_status_wire::plan_label`] — and only when `rate_limit_tier` names
    /// no multiplier, which outranks it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seat_tier: Option<String>,
    /// The plan's own name as the provider shows it, verbatim
    /// (`organization.plan_display_name`, "Ultra" on a Max-20x org, measured
    /// 2026-11-01). Optional per org: a Team org's profile has no such key. When
    /// present it IS the label [`tcr_status_wire::plan_label`] returns; when
    /// absent the label is derived from the three fields above. Overwritten on a
    /// fresh profile read and never blanked by a `None`, like them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_display_name: Option<String>,
    /// Which node this account's requests leave from. See [`Egress`]; absent is
    /// [`Egress::Local`], which is what every account did before this key
    /// existed.
    ///
    /// Skipped when it is `Local`, which is why [`egress_is_local`] exists: a
    /// save rewrites every account row, and without this the first save after
    /// an upgrade writes `"egress": "local"` onto rows whose operator never
    /// asked for a pin, on a file a human reads and edits.
    #[serde(default, skip_serializing_if = "egress_is_local")]
    pub egress: Egress,
    /// Whether a pin that cannot be honoured refuses the request instead of
    /// falling back to the local path. See [`EgressPin::strict`] for what each
    /// answer is for.
    ///
    /// Skipped when false, for the reason above.
    #[serde(
        default,
        rename = "egressStrict",
        skip_serializing_if = "std::ops::Not::not"
    )]
    pub egress_strict: bool,
    /// The folder a Codex row's login was imported from (`tcr login --from-codex`, the auth
    /// file's directory, absolute), for guarded token sharing and TcrBar's re-import. Absent for a login
    /// of tcr's own (`tcr login --codex`), which shares no folder and so leaves nothing to
    /// re-import from, and on every Anthropic row, so an Anthropic-only config re-saves without
    /// the key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_home: Option<String>,
    /// Any per-account keys we do not model (e.g. `models`, `upstream`, `sx`).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Account {
    /// An OAuth account holding only what it cannot exist without: a name, the
    /// upstream it belongs to and an access token. Every other field is unset,
    /// so a caller names the ones it knows with struct update syntax
    /// (`Account { refresh_token, ..Account::new(..) }`), and a field added to
    /// [`Account`] costs one edit here.
    ///
    /// Not `Default`: an account with an empty name and token is not a valid
    /// account, and must not be what a forgotten field silently gets.
    pub fn new(
        name: impl Into<String>,
        provider: crate::provider::Provider,
        access_token: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            account_type: AccountType::OAuth,
            provider,
            account_uuid: None,
            org_uuid: None,
            org_name: None,
            access_token: access_token.into(),
            refresh_token: None,
            expires_at: None,
            priority: None,
            switch_threshold: None,
            disabled: None,
            groups: None,
            organization_type: None,
            rate_limit_tier: None,
            seat_tier: None,
            plan_display_name: None,
            egress: Egress::Local,
            egress_strict: false,
            codex_home: None,
            extra: Map::new(),
        }
    }

    /// A strict peer exit cannot be honoured for Codex. Keep the row readable so
    /// the CLI can clear its pin, but never select it for a pooled request.
    pub(crate) const fn has_unsupported_strict_peer_exit(&self) -> bool {
        match self.provider {
            crate::provider::Provider::Anthropic => false,
            crate::provider::Provider::Codex => self.egress_strict && self.egress.peer().is_some(),
        }
    }

    /// Whether this account belongs to `group`. An account with no `groups` key
    /// belongs to none — it is reachable only by ungrouped routing.
    pub fn in_group(&self, group: &str) -> bool {
        self.groups
            .as_deref()
            .is_some_and(|groups| groups.iter().any(|g| g == group))
    }

    /// This account's exit lock: where it leaves from, and what an exit it
    /// cannot reach costs.
    ///
    /// One call and one typed value, so no consumer re-derives the pin with a
    /// filter of its own: the request path asks this question once per attempt
    /// and reads [`EgressPin`] from then on. Both halves are already parsed by
    /// the time this runs, at config load, which is what makes it infallible.
    pub const fn egress_pin(&self) -> EgressPin {
        EgressPin {
            egress: self.egress,
            strict: self.egress_strict,
        }
    }
}

/// One account's exit lock, as the request path reads it.
///
/// Two values in one type because they are one decision: where this account
/// leaves from, and what it costs when that route is unavailable. Reading them
/// separately is how a caller ends up honouring the pin and forgetting the
/// strictness, which is the case that silently sends an allow-listed account
/// out of the wrong address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EgressPin {
    /// Which node this account's requests leave from.
    pub egress: Egress,
    /// Whether a pin that cannot be honoured refuses the request rather than
    /// falling back to the local path.
    ///
    /// `false`, the default, keeps the request working and says so in one log
    /// line: the pin is a preference about which address the origin sees, and
    /// an operator who has not said otherwise would rather be served from the
    /// wrong address than not served. `true` is the operator who pinned the
    /// account BECAUSE the address is load-bearing, an allow-listed office IP
    /// or a session an origin ties to one address, for whom leaving by another
    /// route is worse than a refusal.
    pub strict: bool,
}

/// Where one account's requests leave the mesh from: the exit lock of
/// decisions row 15, "we need to be able to lock where accounts go out from
/// which server sometimes it might be important for saving the same ip".
///
/// # Why this is about an account and not about the machine
///
/// `tcr-peers.json` already carries [`crate::peer::config::ViaSetting`], which
/// answers a different question: how this Mac reaches the internet when its own
/// path is dead. This one answers where ONE account leaves from on every
/// request, working local path or not. The two never merge, and a reader who
/// takes one for the other gets a pin that only applies during an outage.
///
/// # The file shape is two spellings and no third
///
/// `"egress": "local"`, or `"egress": "via <peer id>"`, which is what
/// `docs/configuration.md`'s account table and `docs/peers.md` both state. The
/// `via ` prefix is what stops a bare 52-character token in a config file being
/// read as an identity by inspection: the key says what kind of thing follows
/// it, so a value that is neither spelling is an error at LOAD rather than a
/// pin nobody can account for. Parsed once, by serde, into this enum. Nothing
/// downstream ever compares it against a string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Egress {
    /// Out of this Mac's own socket, which is what every account did before
    /// this key existed.
    #[default]
    Local,
    /// Always out of this pinned Mac, carried blind, and never another.
    Via(tcr_peer_wire::PeerId),
}

/// `skip_serializing_if` for [`Account::egress`].
///
/// Separate from [`Egress::is_local`] because serde hands the predicate a
/// reference and that method takes the value, which is `Copy`: one adapter
/// here rather than a second spelling of the question.
const fn egress_is_local(egress: &Egress) -> bool {
    egress.is_local()
}

impl Egress {
    /// The word the file stores for "not pinned".
    pub const LOCAL: &'static str = "local";

    /// What a pinned value starts with, so the two spellings cannot be
    /// confused for each other or for anything else. The trailing space is part
    /// of it: `via <peer id>` is the form the documentation states, and a
    /// colon or a bare id is refused rather than quietly accepted beside it.
    pub const VIA_PREFIX: &'static str = "via ";

    /// Whether this account is unpinned. Named rather than `== Local` so every
    /// reader asks the question the same way.
    pub const fn is_local(self) -> bool {
        matches!(self, Self::Local)
    }

    /// The peer this account is pinned to, when it is pinned at all.
    pub const fn peer(self) -> Option<tcr_peer_wire::PeerId> {
        match self {
            Self::Local => None,
            Self::Via(peer) => Some(peer),
        }
    }
}

impl std::fmt::Display for Egress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Local => f.write_str(Self::LOCAL),
            Self::Via(peer) => write!(f, "{}{}", Self::VIA_PREFIX, peer.to_wire()),
        }
    }
}

impl From<Egress> for String {
    fn from(egress: Egress) -> Self {
        egress.to_string()
    }
}

impl TryFrom<String> for Egress {
    type Error = String;

    /// Two spellings and no third, and a value that is neither fails the whole
    /// config load.
    ///
    /// Refusing here is the point. A pin the operator typed wrong, read as
    /// `local`, would send the account out of the very address it was written
    /// down to avoid, and it would do it silently on every request; a load that
    /// stops and names the value is the one outcome that cannot do that.
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        let raw = raw.trim();
        if raw == Self::LOCAL {
            return Ok(Self::Local);
        }
        let Some(id) = raw.strip_prefix(Self::VIA_PREFIX) else {
            return Err(format!(
                "account `egress` must be `{}` or `{}<peer id>`, and `{raw}` is neither \
                 (`tcr peer ls` prints the ids this Mac has pinned)",
                Self::LOCAL,
                Self::VIA_PREFIX,
            ));
        };
        tcr_peer_wire::PeerId::parse(id.trim())
            .map(Self::Via)
            .map_err(|err| {
                format!(
                    "account `egress` is pinned to `{id}`, which is not a peer id \
                     (`tcr peer ls` prints the ids this Mac has pinned): {err}"
                )
            })
    }
}

/// Properties of one group label (`groupSettings.<name>` in the file) — a home
/// for facts about the GROUP itself, distinct from `Account::groups`, which only
/// records membership. Absent for a group name that no account carries a
/// `groupSettings` entry for; `#[serde(default)]` so an older config (or a group
/// with no properties set) round-trips untouched.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GroupSettings {
    /// When `true`, this group is additionally private in the INBOUND
    /// direction: an account carrying it is off-limits to traffic that did not
    /// ask for one of its groups — see [`crate::manager::select::eligible`] for
    /// the exact rule.
    ///
    /// This used to be the ONLY way to get the OUTBOUND half too — an explicit
    /// `--group` ask for this group refusing rather than spilling into the pool
    /// when no member can serve. That coupling is gone: the outbound half is
    /// now the DEFAULT for every `--group` ask, reserved or not (see
    /// [`Self::spill_to_pool`] for the escape hatch), so `reserved` today means
    /// only the inbound half — keeping OTHER traffic out. It does not loosen
    /// the outbound half; a reserved group with `spillToPool` unset is still
    /// strict on its own traffic, same as any other group.
    ///
    /// The split exists because the old single flag produced a group that
    /// looked private while still leaking: measured on the live fleet
    /// 2026-09-01, an unreserved group's member absorbed 127 pool diverts in a
    /// day, and the rate-limit that caused then spilled that group's own
    /// traffic 33 times. Closing only the inbound leak (this field) was not
    /// enough — the outbound leak needed its own default, which is why strict
    /// output is now universal instead of tied to this flag.
    ///
    /// Absent/`false` (default) admits unrequested traffic; `--group` strictness
    /// is governed separately, by [`Self::spill_to_pool`].
    #[serde(default)]
    pub reserved: bool,
    /// When `true`, every account carrying this group is OUT OF ROTATION, the
    /// same way a per-account `disabled` is: it is never selected, never
    /// pinned, and an established pin on it is re-keyed away. One key holds a
    /// whole group out and one key releases it, which is the whole point —
    /// the alternative an operator reaches for otherwise is `tcr disable` per
    /// member plus a label to remember which ones.
    ///
    /// Independent of [`Self::reserved`] and of a member's own `disabled`:
    /// a group can be both parked and reserved, and unparking a group does not
    /// re-enable a member that was disabled by hand. Absent/`false` (default)
    /// is ordinary rotation.
    #[serde(default)]
    pub parked: bool,
    /// When `true`, an explicit `--group` ask for this group is allowed to
    /// select the control account — see
    /// [`crate::manager::select::select_with_group`]'s `control_excluded`
    /// handling for the exact carve-out. Absent/`false` (default) keeps
    /// today's behaviour: inference never selects the control account, so a
    /// group whose only member is the control account can never route.
    #[serde(default)]
    pub allow_control_account: bool,
    /// When `true`, an explicit `--group` ask for this group is allowed to
    /// SPILL: when no member can serve, the request falls back to the whole
    /// pool instead of refusing — the pre-2026-09-13 behaviour, kept as an
    /// opt-in escape hatch. Absent/`false` (default) is STRICT: the ask
    /// refuses rather than spilling, and the caller's exhaustion ladder
    /// soft-waits for a member or answers an honest 429 — see
    /// [`crate::manager::select::select_with_group`]'s `strict_group` handling
    /// for the exact rule.
    ///
    /// Independent of [`Self::reserved`]: a reserved group ignores this field
    /// and is always strict, because spilling a reserved group's own traffic
    /// into the pool would contradict the inbound half of what `reserved`
    /// promises — an account walled off from other traffic is not a sane
    /// landing spot for traffic that could not reach it any other way. This
    /// field only ever widens an UNRESERVED group back to the pre-2026-09-13
    /// default.
    #[serde(default)]
    pub spill_to_pool: bool,
    /// A configured `#RRGGBB` (or `#RGB`, normalized on write) color for this
    /// group's panel tag. Absent → no color was ever set, and
    /// [`Config::group_color`] derives one deterministically from the group
    /// name instead — see that function's doc-comment for the band. Never
    /// read directly by a consumer that wants "the" color for a group; go
    /// through [`Config::group_color`] / [`Config::group_colors`] so derived
    /// and configured colors resolve through one seam.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Any `groupSettings.<name>.*` keys we do not model.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Per-account request pacing (opt-in; default OFF).
///
/// Both knobs are `Option`: absent in the config file → `None` → pacing is inert,
/// so an unconfigured proxy behaves byte-for-byte as before. When set, pacing can
/// only ever DELAY/SPREAD selection across the fleet — never turn a servable
/// request into a failure (the soft fallback in [`crate::manager::Manager::select`]).
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PacingConfig {
    /// Cap on requests concurrently being served on one account. An account at or
    /// over the cap is temporarily skipped in selection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_in_flight_per_account: Option<u32>,
    /// Minimum gap (ms) between two selects of the SAME account. An account
    /// selected less than this ago is temporarily skipped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_spacing_ms: Option<u64>,
}

impl PacingConfig {
    /// The in-flight cap, treating a configured `0` as "disabled" (identical to
    /// leaving it unset). A literal `Some(0)` would make `in_flight >= 0` true for
    /// every account, holding out the ENTIRE fleet permanently — collapsing every
    /// request onto the least-loaded soft fallback and flooding the pacing log with
    /// a "skip in selection" line per account per request. Normalising it here keeps
    /// that footgun out of every read site.
    pub const fn effective_max_in_flight(&self) -> Option<u32> {
        match self.max_in_flight_per_account {
            Some(0) => None,
            other => other,
        }
    }

    /// Whether either knob is configured. When `false`, pacing is fully inert and
    /// eligibility/selection are byte-identical to the no-pacing build. A cap of
    /// `0` counts as unset (see [`Self::effective_max_in_flight`]).
    pub const fn is_active(&self) -> bool {
        self.effective_max_in_flight().is_some() || self.min_spacing_ms.is_some()
    }
}

/// Knobs for one GCRA token bucket over the upstream send site: `burst` requests
/// admit instantly after idle, then one per `minSpacingMs`.
///
/// The same shape is used for BOTH throttle layers, which differ only in what they
/// are keyed on and how tight they are:
///
/// * [`Config::account_throttle`] — keyed PER ORGANIZATION. This is the real
///   limiter. Anthropic sets limits at the organization level, so pacing per-org is
///   what actually matches the thing being limited.
/// * [`Config::fleet_throttle`] — one bucket for everything. Insurance only, set far
///   looser (see [`default_fleet_throttle`]).
///
/// **On the 350ms.** It originates as [`crate::probe::PROBE_SPACING`], proven on
/// `/api/oauth/usage` — the usage-probe endpoint, NOT `/v1/messages`. It was
/// transplanted onto the serve path as a starting value and has never been
/// validated there. Treat it as a conservative unknown, not a measured limit;
/// `docs/plans/throttle-live-sweep-runbook.md` is the (still unrun) procedure that
/// would pin the real number.
///
/// **What was here before, and how far the evidence actually goes.** This
/// doc-comment used to assert that the fleet-wide bucket paced "the AGGREGATE
/// egress that Anthropic's shared `IP/client_id` burst limiter actually keys on,"
/// with no evidence attached. It is still uncited, and it is still the reason the
/// bucket was shaped the way it was — but be careful about how much the
/// counter-evidence proves:
///
/// * Anthropic documents limits at the ORGANIZATION level, and this fleet's own
///   readings show N accounts reporting N different utilizations. That is solid
///   evidence that the QUOTA COUNTER is per-organization.
/// * It is **not** evidence against a shared per-IP or per-client_id BURST
///   limiter. Those are different mechanisms at different layers and coexist at
///   essentially every CDN-fronted API. A first draft of this comment used the
///   first bullet to refute the second claim; that was a non-sequitur.
/// * The documented org-level limits describe Console/API-key organizations.
///   These are Max SUBSCRIPTION OAuth accounts, whose quota arrives on
///   `anthropic-ratelimit-unified-*` headers that appear nowhere in the public
///   rate-limit documentation. Transferring the org-scoping conclusion across
///   that boundary is plausible, not proven.
///
/// So: per-org pacing is the right PRIMARY shape on the evidence available, and
/// whether any shared-identity limit exists remains genuinely unmeasured. That
/// unmeasured risk is exactly what [`Config::fleet_throttle`] is for, and why it
/// is tuned to a value it can actually reach rather than a decorative one.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ThrottleConfig {
    /// Steady-state emission interval T (ms): after the burst budget is spent, at
    /// most one upstream send is initiated per this many ms **within whatever this
    /// bucket is keyed on** — one organization for `accountThrottle`, one
    /// provider's fleet for `fleetThrottle`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_spacing_ms: Option<u64>,
    /// Bucket capacity B: how many sends may fire instantly after an idle period.
    /// Absent → treated as 1 (strict spacing).
    ///
    /// Set it ABOVE the fan-out you want to pass untaxed and BELOW the one you want
    /// paced. For `accountThrottle` that is one session's within-turn fan-out
    /// (inference + `count_tokens` + telemetry); for `fleetThrottle` it is the
    /// widest agent wave you consider normal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<u32>,
}

impl ThrottleConfig {
    /// Emission interval, treating `Some(0)` as unset (mirrors
    /// [`PacingConfig::effective_max_in_flight`]'s footgun normalization).
    pub const fn effective_min_spacing(&self) -> Option<u64> {
        match self.min_spacing_ms {
            Some(0) => None,
            other => other,
        }
    }
    /// Bucket capacity, clamped to >= 1 (B=1 ⇒ strict min-spacing).
    pub fn effective_burst(&self) -> u32 {
        self.burst.unwrap_or(1).max(1)
    }
    /// Whether the throttle does anything. `min_spacing_ms` is the required knob —
    /// a burst without a spacing interval is meaningless. When false the throttle is
    /// fully inert (see [`crate::manager::Manager::throttle_send`]).
    pub const fn is_active(&self) -> bool {
        self.effective_min_spacing().is_some()
    }
}

/// The `controlIdentity` key: what the proxy does when a client's own OAuth
/// identity — resolved once per bearer via `/api/oauth/profile` — is not the
/// control account. See [`crate::control_identity`] for why this is a CHECK
/// rather than a routing decision: the client's login, not the proxy, decides
/// which account every connector, plugin and setting comes from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ControlIdentityMode {
    /// Answer a mismatched client with a 403 naming both identities and the
    /// fix, so a session on the wrong account fails at its first request
    /// instead of running for hours with the wrong connectors. Intended to
    /// become the default once a release of `warn` has shown how common the
    /// drift is; a headless `tcr run` worker sharing a drifted keychain would
    /// otherwise be refused with nothing on any screen saying why.
    Refuse,
    /// Serve the request, log the mismatch once per bearer. The default.
    #[default]
    Warn,
    /// Do not resolve client identities at all.
    Off,
}

impl ControlIdentityMode {
    /// The `controlIdentity` spelling, for log lines.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Refuse => "refuse",
            Self::Warn => "warn",
            Self::Off => "off",
        }
    }
}

/// The `controlAccount` key on disk: an account `name`, or the identity object
/// (`name`/`accountUuid`/`orgUuid`/`orgName`) a `tcr` from the duplicated-email
/// era wrote. `#[serde(untagged)]` is unambiguous here because the two shapes
/// are structurally disjoint: a JSON string can never parse as the object
/// variant and vice versa.
///
/// A name is the whole key now — `Account.name` is unique across a config by
/// construction (see [`crate::identity`]) — so [`save_control_account`] always
/// writes the STRING, and the object shape survives only as something older
/// configs are read back from. Reading one costs nothing and collapses it to
/// its name on the next write.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged, rename_all = "camelCase")]
pub enum ControlAccountRef {
    /// The account's `name`, which resolves to exactly one row.
    Name(String),
    /// The identity object a `tcr` between `8411f3e` and this change wrote, when
    /// a name could name two rows. Accepted, never written; only `name` is read
    /// from it.
    ///
    /// `rename_all = "camelCase"` is repeated here, not just at the enum
    /// level, because serde's container `rename_all` renames VARIANT names for
    /// an enum, not the FIELDS of a struct variant — the two are separate
    /// attribute targets and only the variant-level one reaches these fields.
    #[serde(rename_all = "camelCase")]
    Identity {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account_uuid: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        org_uuid: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        org_name: Option<String>,
    },
}

impl ControlAccountRef {
    /// The account name, present in both shapes.
    pub fn name(&self) -> &str {
        match self {
            Self::Name(name) => name,
            Self::Identity { name, .. } => name,
        }
    }
}

impl From<&Account> for ControlAccountRef {
    /// Always the NAME — see this type's doc-comment.
    fn from(account: &Account) -> Self {
        Self::Name(account.name.clone())
    }
}

/// Top-level config document.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    #[serde(default)]
    pub proxy: ProxyConfig,
    #[serde(default = "default_upstream")]
    pub upstream: String,
    /// Where Codex accounts' requests go. Separate from `upstream` because the two providers are
    /// different hosts and tests point each at its own stub. Written to disk only when the user
    /// changed it (see `is_default_codex_upstream`): an upgraded Anthropic-only file gains no
    /// `codexUpstream` key.
    #[serde(
        default = "default_codex_upstream",
        skip_serializing_if = "is_default_codex_upstream"
    )]
    pub codex_upstream: String,
    #[serde(default = "default_switch_threshold")]
    pub switch_threshold: f64,
    /// Override for the Fable weekly bucket (`7d_oi`) alone. `None` (absent from
    /// the JSON) means "same as `switchThreshold`" — today's behaviour,
    /// byte-for-byte. `1.0` means "gate Fable only on a genuine upstream
    /// rejection", never on tcr's own prediction — see
    /// [`crate::quota::Quota::model_weekly_rejected`]'s doc for why 1.0 is the
    /// value that collapses the prediction onto the authority. Consumed by
    /// [`crate::manager::select::Manager::model_blocked`] and
    /// [`crate::manager::select::Manager::account_gate`]'s `FableWeekly` push
    /// through one shared resolver so the two can never disagree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fable_weekly_threshold: Option<f64>,
    /// Per-account request pacing. Absent in JSON → [`default_pacing`] → all knobs
    /// `None`, i.e. OFF: a per-account concurrency cap trades prompt-cache locality
    /// for load spread, and on a single-user proxy the cache is the scarce resource.
    /// Set `"pacing": {"maxInFlightPerAccount": N}` to opt back in. The global
    /// [`ThrottleConfig`] is a RATE limiter and is deliberately not a substitute for
    /// a concurrency bound.
    #[serde(default = "default_pacing")]
    pub pacing: PacingConfig,
    /// Per-ORGANIZATION outbound throttle — the primary limiter. Absent →
    /// [`default_account_throttle`] (ON: `minSpacingMs: 350, burst: 24`). Set
    /// `"accountThrottle": {}` to disable it (all knobs `None`). Read at boot.
    #[serde(default = "default_account_throttle")]
    pub account_throttle: ThrottleConfig,
    /// Ceiling across every organization of one provider — insurance, not the primary
    /// limiter. Each provider has its own bucket, so Claude and Codex never pace
    /// each other through this ceiling. Absent → [`default_fleet_throttle`] (ON: `minSpacingMs: 100,
    /// burst: 32`). Set `"fleetThrottle": {}` to disable it. Read at boot.
    ///
    /// Disabling BOTH is what the old `"throttle": {}` escape hatch used to mean.
    /// Neither key on its own leaves the proxy unthrottled — that asymmetry is why
    /// migrating a stale `throttle` key needs two branches rather than one: no
    /// single new key means "unthrottled", so an inert legacy `throttle` has to map
    /// onto BOTH buckets set inert, and only a migration that special-cases that
    /// can preserve what the operator meant (see [`load`]).
    #[serde(default = "default_fleet_throttle")]
    pub fleet_throttle: ThrottleConfig,
    /// Hard account lock: when set to an account `name`, its provider's pooled
    /// traffic is pinned to that exact account slot. The other provider rotates
    /// normally. LRU rotation, session affinity, and load-balancing migration are
    /// bypassed for the locked provider. Absent → normal routing (default). Tradeoff:
    /// a locked account has NO failover; if it is throttled/disabled/down, requests
    /// fail rather than rotating. Set to the exact `accounts[].name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock_account: Option<String>,
    /// The identity-bound control account: the one account that
    /// `_tcr/accounts/control` resolves to and that stays PROBEABLE (usage
    /// tracked) even while `disabled`. Unlike [`Self::lock_account`] this does
    /// NOT change selection by itself — see the manager's `control_idx` doc for
    /// the resolution and the routing this key only sets up for (part 2).
    /// Absent → no control account (default). `skip_serializing_if` so clearing
    /// it REMOVES the key rather than writing `null` — same contract as
    /// `lock_account` would want if it grew a setter.
    ///
    /// [`ControlAccountRef`] accepts a legacy bare name (every config on disk
    /// before this change) or an identity object; boot resolution refuses to
    /// guess when a bare name is duplicated rather than silently binding to
    /// whichever row sorts first — see `crate::manager::Manager::assemble`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_account: Option<ControlAccountRef>,
    /// Quota headroom reserved for the control account against GENERAL
    /// (non-control-preferred) picks — part of the routing half (part 2) of the
    /// control-account feature. `threshold - control_reserve` is the effective
    /// switch threshold a general pool pick applies to the control account
    /// specifically; a control-preferred pick still uses the full threshold.
    /// Absent → 0.05. Clamped to `[0.0, 0.5]` by the manager at construction — a
    /// value outside that range in a hand-edited file is not trusted as-is.
    ///
    /// **Live exactly when the control account can be picked at all**, which is
    /// two cases and neither is the default: [`Self::control_pooled`], or an
    /// explicit `--group g` ask where `g` carries
    /// [`GroupSettings::allow_control_account`]. With both off the control
    /// account is force-added to the pick's `tried` set before any quota check
    /// runs, so this never gets consulted. See
    /// [`crate::manager::select::effective_threshold`].
    #[serde(default = "default_control_reserve")]
    pub control_reserve: f64,
    /// Put the control account into GENERAL inference rotation, guarded only by
    /// [`Self::control_reserve`]. Absent → `false` (default), which keeps the
    /// long-standing rule that an inference request never selects the control
    /// account.
    ///
    /// **This trades the control account's availability for its quota, and the
    /// trade can be lost.** The reserve is our own arithmetic over Anthropic's
    /// rate-limit headers, which lag; upstream is the oracle and answers 200s
    /// for accounts it is about to bench. Measured on this fleet 2026-08-29: an
    /// account tracked all the way up still crossed into
    /// `quota.status == "rejected"`, and [`crate::manager::Manager::control_eligible`]
    /// fails closed on `rejected` — so a pooled control account CAN burn itself
    /// out of the identity plane it exists to anchor, and stay out until its
    /// weekly window resets. Enable it when idle quota costs more than that
    /// outage would; leave it off otherwise.
    ///
    /// Inert while the control account is `disabled` — `eligible`'s own terminal
    /// gate excludes it from every general pick first, and this flag deliberately
    /// does not reach past that. `disabled` remains the way to say "identity
    /// plane only", exactly as the control-account design defines it.
    #[serde(default)]
    pub control_pooled: bool,
    /// What the proxy does with a client whose own OAuth identity is not the
    /// control account. Absent → [`ControlIdentityMode::Warn`]. Inert without a
    /// `controlAccount`, and for a request that carries no `Authorization: Bearer`.
    #[serde(default)]
    pub control_identity: ControlIdentityMode,
    /// Width, in hours, of the reset-urgency bucket rotation ranks by within a
    /// priority tier — see [`default_reset_urgency_tier_hours`] for why this is
    /// a bucket and not the raw reset instant. Absent → 24. `0` disables the
    /// term and restores the pre-tier `(priority, lastSelectedSeq, reset)`
    /// ordering.
    ///
    /// **Boot-time snapshot, like every field here except `groups` and
    /// `groupSettings`** — the manager resolves it once at construction, so an
    /// edit needs a restart before the serving process rotates by it. `tcr
    /// status` reports the running process's view, not the file's.
    #[serde(default = "default_reset_urgency_tier_hours")]
    pub reset_urgency_tier_hours: u32,
    /// Force the upstream-forwarding client onto HTTP/1.1 instead of the
    /// h2-and-fall-back-to-h1 negotiation reqwest does by default. Absent →
    /// `false` (h2). OFF by default deliberately: an intermittent
    /// connection-level fault (GOAWAY / framing error) on HTTP/2 kills EVERY
    /// multiplexed stream sharing that connection at once, so one fault takes
    /// down every concurrent session an account happens to be serving. h1
    /// does not multiplex, so a fault there costs exactly one in-flight
    /// request — a structural cap on blast radius, not a statistical one.
    /// The trade: h1 gives up multiplexing, opens one TCP+TLS connection per
    /// concurrent request instead of sharing one, and raises the open-socket
    /// count against Anthropic's edge — real costs, which is why this stays
    /// opt-in. Set `"http1Only": true` to enable.
    #[serde(default)]
    pub http1_only: bool,
    /// Per-model price overrides for the usage ledger's cost figures, keyed by a
    /// model id or any PREFIX of one (`"claude-opus-5"` covers every dated
    /// variant). `input` and `output` are USD per million tokens and are
    /// required; the three cache dimensions default to the same multipliers the
    /// built-in table uses (read 0.1x input, 5-minute write 1.25x, 1-hour write
    /// 2x). An override beats the built-in table outright — see
    /// [`crate::pricing::PricingTable::lookup`].
    ///
    /// This is also how a model the built-in table has no published rate for
    /// gets priced instead of counting as `unpricedRequests`. Absent → the
    /// built-in table alone.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub pricing: HashMap<String, crate::pricing::PricingOverride>,
    /// How many days of usage-ledger day files to keep in
    /// `~/.cache/tcr/usage/`. Absent → 90. Checked once at boot; older
    /// files are deleted then. `0` keeps only today and yesterday (the two the
    /// boot replay reads), which is the floor — the ledger cannot keep less and
    /// still survive a restart, so `crate::usage::prune` refuses to delete
    /// either of those two days whatever this says. A fleet holding a Codex
    /// account replays the six days before those two as well, for its weekly
    /// window, so there the floor is eight days and `0` keeps all eight
    /// (`crate::usage::UsageTracker::attach_ledger`).
    #[serde(default = "default_usage_retention_days")]
    pub usage_retention_days: u32,
    #[serde(default)]
    pub accounts: Vec<Account>,
    /// Properties of GROUPS themselves (`"groupSettings": {"codereview":
    /// {"reserved": true}}`), keyed by the same label `Account::groups` carries.
    /// Absent → no group is reserved (default). Membership stays per-account;
    /// this map holds only properties, so a key here naming a group with no
    /// members is harmless — it reserves nothing — and is deliberately NOT
    /// validated against current membership.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub group_settings: HashMap<String, GroupSettings>,
    /// Any top-level keys we do not model, preserved verbatim on save.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
    /// Names of `accounts[]` entries [`load`] dropped because they carried no
    /// usable credential ([`unusable_account`]) — see `load`'s doc-comment.
    /// Populated ONLY by `load`; `#[serde(skip)]` so it is never read from or
    /// written to the file — a `save()`/`save_tokens()` round-trip of a
    /// `Config` carrying quarantined names must not fabricate an `accounts[]`
    /// entry for them, and must not accept one from a hand-edited file either.
    /// Empty for a `Config` built any other way (a test's
    /// `serde_json::from_str`, `Config::default()`-shaped construction).
    #[serde(skip)]
    pub quarantined_accounts: Vec<String>,
    /// Set when [`load`] migrated a legacy `throttle` key into
    /// `accountThrottle`/`fleetThrottle` on this load. `#[serde(skip)]` — never
    /// read from or written to the file, same pattern as
    /// [`Self::quarantined_accounts`]. The boot path in `main.rs` reads this to
    /// decide whether to persist the migrated document back to disk once, so a
    /// migrated install self-heals instead of re-warning on every start.
    #[serde(skip)]
    pub migrated_legacy_throttle: bool,
    /// The renames [`load`] applied on this load because two `accounts[]`
    /// entries carried the same `name`. Empty on every load of an already-unique
    /// config, which after the first migration is every load.
    ///
    /// `#[serde(skip)]`, same pattern and same reason as
    /// [`Self::quarantined_accounts`]. Read by the boot path to rewrite the
    /// session-affinity pin file, which is keyed by name, so warm sessions
    /// survive the migration.
    #[serde(skip)]
    pub renamed_accounts: Vec<AccountRename>,
    /// Why the durable half of the rename migration did not happen, when it did
    /// not. The in-memory `accounts` are renamed either way — a server must boot
    /// — but a CLI verb about to WRITE this config refuses on it, because a
    /// second process would re-derive different names from the same duplicates.
    #[serde(skip)]
    pub rename_write_error: Option<String>,
}

/// One account [`load`] renamed to restore unique names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountRename {
    /// The duplicated name the entry carried on disk.
    pub from: String,
    /// The unique name it now carries, minted by [`crate::identity::mint_name`].
    pub to: String,
    /// The org the new name was derived from, for the log line. `None` when the
    /// entry stored no org at all and the name fell through to a counter.
    pub org: Option<String>,
}

impl Config {
    /// Whether session affinity is on, read from the unmodelled top-level
    /// `sessionAffinity` (default `true`: ON unless the file says
    /// `"sessionAffinity": false`). The one reader: the manager's
    /// [`crate::manager::Manager::session_affinity_enabled`] and `tcr config
    /// show` both call this, so the panel can never report a value the server
    /// does not use. Why the default is ON is on the manager's method.
    pub fn session_affinity_enabled(&self) -> bool {
        self.extra
            .get("sessionAffinity")
            .and_then(Value::as_bool)
            .unwrap_or(true)
    }

    /// [`Self::control_reserve`] as the manager applies it: clamped to
    /// `[0.0, 0.5]`, because a hand-edited file is not bound by the field's
    /// documented range. Read once at manager construction and by `tcr config
    /// show`.
    pub const fn effective_control_reserve(&self) -> f64 {
        self.control_reserve.clamp(0.0, 0.5)
    }

    /// The fleet bucket as `(burst, spacing_ms)` when the file sets a burst
    /// and the spacing is on ([`ThrottleConfig::effective_min_spacing`]);
    /// `None` otherwise. The load test bounds the stub's arrivals with it.
    pub const fn fleet_bucket(&self) -> Option<(u32, u64)> {
        match (
            self.fleet_throttle.burst,
            self.fleet_throttle.effective_min_spacing(),
        ) {
            (Some(burst), Some(spacing)) => Some((burst, spacing)),
            _ => None,
        }
    }

    /// The Fable weekly threshold the fleet-wide gate applies: the override
    /// when the file sets one, otherwise [`Self::switch_threshold`]. Resolved
    /// through the manager's own resolver, so this and the selection path
    /// cannot disagree.
    pub fn effective_fable_weekly_threshold(&self) -> f64 {
        crate::manager::effective_fable_threshold(
            self.fable_weekly_threshold,
            self.switch_threshold,
        )
    }

    /// The set of group labels currently marked `reserved`. Cheap to call —
    /// intended for one-shot use (CLI verbs, [`crate::manager::Manager`]
    /// construction caching its own copy), not a per-request hot path.
    pub fn reserved_group_names(&self) -> HashSet<String> {
        self.group_settings
            .iter()
            .filter(|(_, s)| s.reserved)
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Whether `group` is currently marked `reserved`. `false` for a group
    /// name with no `groupSettings` entry at all — same "absent means not
    /// reserved" default the field's own doc-comment promises.
    pub fn is_group_reserved(&self, group: &str) -> bool {
        self.group_settings.get(group).is_some_and(|s| s.reserved)
    }

    /// The set of group labels currently marked `parked`. Same cheap,
    /// one-shot-use posture as [`Self::reserved_group_names`].
    pub fn parked_group_names(&self) -> HashSet<String> {
        self.group_settings
            .iter()
            .filter(|(_, s)| s.parked)
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Whether `group` is currently marked `parked`. `false` for a group name
    /// with no `groupSettings` entry at all — same "absent means not parked"
    /// default the field's own doc-comment promises.
    pub fn is_group_parked(&self, group: &str) -> bool {
        self.group_settings.get(group).is_some_and(|s| s.parked)
    }

    /// The set of group labels currently opted in to `allowControlAccount`.
    /// Same cheap, one-shot-use posture as [`Self::reserved_group_names`].
    pub fn control_allowed_group_names(&self) -> HashSet<String> {
        self.group_settings
            .iter()
            .filter(|(_, s)| s.allow_control_account)
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Whether `group` currently opts in to selecting the control account.
    /// `false` for a group name with no `groupSettings` entry at all — same
    /// "absent means false" default the field's own doc-comment promises.
    pub fn group_allows_control(&self, group: &str) -> bool {
        self.group_settings
            .get(group)
            .is_some_and(|s| s.allow_control_account)
    }

    /// The set of group labels currently opted in to `spillToPool` — an
    /// explicit `--group` ask for one of these falls back to the whole pool
    /// on exhaustion instead of refusing. Same cheap, one-shot-use posture as
    /// [`Self::reserved_group_names`].
    pub fn spill_group_names(&self) -> HashSet<String> {
        self.group_settings
            .iter()
            .filter(|(_, s)| s.spill_to_pool)
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Whether `group` currently opts in to `spillToPool`. `false` for a group
    /// name with no `groupSettings` entry at all — same "absent means false"
    /// default the field's own doc-comment promises. Callers deciding
    /// strictness must also check [`Self::is_group_reserved`] — a reserved
    /// group ignores this and is always strict; see [`GroupSettings::spill_to_pool`]'s
    /// doc-comment.
    pub fn group_allows_spill(&self, group: &str) -> bool {
        self.group_settings
            .get(group)
            .is_some_and(|s| s.spill_to_pool)
    }

    /// Every group label that currently has at least one member — the same
    /// "a group exists only while some account carries the label" definition
    /// [`crate::cli::remove_from_group`]'s `--all` arm already uses. A
    /// `groupSettings` entry for a name no account carries (color configured
    /// ahead of membership, or the last member since removed) is deliberately
    /// NOT included: [`Self::group_colors`] answers "what should the panel
    /// tag right now", not "what has ever been configured".
    pub fn all_group_names(&self) -> BTreeSet<String> {
        self.accounts
            .iter()
            .filter_map(|a| a.groups.as_ref())
            .flatten()
            .cloned()
            .collect()
    }

    /// The resolved color for `group` plus whether it was configured or
    /// derived. Every group gets a color from this whether or not one was
    /// ever set — see [`derive_group_color`]'s doc-comment for the
    /// hue/saturation/lightness band and why it must be picked HERE, once,
    /// rather than left for each client to invent independently.
    pub fn group_color(&self, group: &str) -> (String, ColorSource) {
        match self
            .group_settings
            .get(group)
            .and_then(|s| s.color.as_deref())
        {
            Some(hex) => (hex.to_string(), ColorSource::Set),
            None => (derive_group_color(group), ColorSource::Derived),
        }
    }

    /// Every group that exists on the fleet ([`Self::all_group_names`])
    /// mapped to its resolved color ([`Self::group_color`]) — the exact
    /// shape `groupColors` puts on the wire. A `BTreeMap` so the wire order
    /// (and any snapshot/test comparing it) is deterministic run to run,
    /// independent of `HashMap` iteration order.
    pub fn group_colors(&self) -> BTreeMap<String, String> {
        self.all_group_names()
            .into_iter()
            .map(|g| {
                let (hex, _) = self.group_color(&g);
                (g, hex)
            })
            .collect()
    }

    /// How many ENABLED accounts would remain reachable by traffic that asks
    /// for nothing, if exactly the group names in `reserved` were reserved —
    /// the safety-rail figure `tcr group reserve` prints and refuses on. Takes
    /// the candidate set explicitly (rather than reading
    /// [`Self::reserved_group_names`] itself) so a caller can ask "what WOULD
    /// this leave" before writing the change.
    pub fn unreserved_enabled_count(&self, reserved: &HashSet<String>) -> usize {
        self.accounts
            .iter()
            .filter(|a| !a.disabled.unwrap_or(false))
            .filter(|a| {
                a.groups
                    .as_deref()
                    .unwrap_or(&[])
                    .iter()
                    .all(|g| !reserved.contains(g))
            })
            .count()
    }

    /// Whether any account in this config is a Codex account, in any state
    /// (disabled, grouped, pinned, or not).
    ///
    /// The gate every Codex-aware CLI sentence sits behind: a fleet for which
    /// this is false is Claude-only, and prints exactly what it printed before
    /// Codex accounts existed. Reads [`Self::accounts`], so a row the loader set
    /// aside never counts, and neither does one already deleted from the file.
    pub fn holds_codex(&self) -> bool {
        self.accounts
            .iter()
            .any(|a| a.provider == crate::provider::Provider::Codex)
    }

    /// [`Self::unreserved_enabled_count`], counting only `provider`'s accounts.
    ///
    /// The two pools are counted apart because neither can serve the other's
    /// traffic: a Claude session is never served by a Codex account, so a
    /// reservation that leaves only Codex accounts unreserved leaves Claude's
    /// unrequested traffic with nothing, however many accounts remain.
    pub fn unreserved_enabled_count_for(
        &self,
        reserved: &HashSet<String>,
        provider: crate::provider::Provider,
    ) -> usize {
        self.accounts
            .iter()
            .filter(|a| a.provider == provider)
            .filter(|a| !a.disabled.unwrap_or(false))
            .filter(|a| {
                a.groups
                    .as_deref()
                    .unwrap_or(&[])
                    .iter()
                    .all(|g| !reserved.contains(g))
            })
            .count()
    }
}

/// The default config path: `$HOME/.config/tcr.json`
/// ([`crate::paths::config_file`]).
///
/// Deliberately NOT the platform config dir (`directories` would pick
/// `~/Library/Application Support` on macOS): every install already keeps it
/// in `~/.config`, beside tcr's CA and peers file.
pub fn default_path() -> PathBuf {
    crate::paths::config_file()
}

#[cfg(test)]
mod tests;
