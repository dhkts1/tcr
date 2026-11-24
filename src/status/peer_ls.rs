//! The `tcr peer ls --json` document.

use serde::{Deserialize, Serialize};

use super::network_fact;
use super::peer_rows::masked_label;

// ---------------------------------------------------------------------------
// The `tcr peer ls --json` document
// ---------------------------------------------------------------------------

/// One pinned Mac as `tcr peer ls --json` reports it.
///
/// # Why this names every field instead of flattening the row
///
/// It used to be `#[serde(flatten)] row: PeerRow`, which put EVERY key the
/// peers file holds on the wire, `rendezvousSecret` included: 64 hex characters
/// of the pair's derived port secret, on a payload TcrBar polls every three
/// seconds and an operator pastes into a bug report. `docs/peers.md` says that
/// value is never sent to anybody. A flattened struct makes "what crosses" a
/// property of another file's struct definition, so the next field added to
/// [`crate::peer::config::PeerRow`] crosses too, silently.
///
/// Naming the fields moves that decision here and makes it a compile-time one:
/// a new field on `PeerRow` reaches this payload only when somebody writes it
/// down. The secret stays in the peers FILE, which is where the reach code
/// reads it from; nothing on this surface needs it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerLsRow {
    /// The pinned static key, wire form (52 Crockford characters). The same
    /// form [`PeerStatusRow::id`] carries, so the panel can join the two reads.
    pub node: tcr_peer_wire::PeerId,
    /// The operator's label, through [`masked_label`].
    pub label: String,
    /// Every way this Mac knows to reach that one, newest first.
    #[serde(default)]
    pub endpoints: Vec<crate::peer::config::Endpoint>,
    /// When the pin was taken, Unix milliseconds.
    pub added_at: i64,
    /// What that Mac may do here, and what this Mac may ask of it.
    pub allow: crate::peer::config::Allow,
    /// The operator's own grants for this row, one per lease.
    #[serde(default)]
    pub lend: Vec<crate::peer::config::LendGrant>,
    /// The address that peer last said it sees THIS Mac at, and when it said
    /// so. An address is not a secret, and the panel draws it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sees_us_at: Option<(String, u64)>,
    /// The row-level end over every lease this row is party to, Unix SECONDS,
    /// `null` when nothing this row is party to has an end. While any lease is
    /// running it is the SOONEST end among the live ones, the instant a row's
    /// "ends in 1 h" counts down to; once they have all ended it is the LATEST
    /// end, the "ended <when>" a greyed row prints.
    ///
    /// Derived against the clock at print time and never read off the file, for
    /// the reason [`crate::peer::config::LendGrant::ended`] gives: a lease ends
    /// on a wall clock and the file's mtime does not move when 18:00 arrives.
    /// The per-lease `until` is still on each grant inside [`Self::lend`]; this
    /// is the ROW-level answer, so a row can be drawn without walking its
    /// leases and picking a different one than the next reader would.
    ///
    /// Derived from every lease this row is party to, whichever way it points:
    /// the operator's own grants and the leases the ledger minted against them,
    /// and the leases this Mac has been GRANTED by that row's Mac
    /// (`peer-state.json`'s `borrowed` section). A borrower's row used to read
    /// `until: null` for a Mac that was actively serving it, because the
    /// borrower's copy lived only in a running proxy's memory.
    pub until: Option<u64>,
    /// True only when this row has at least one lease and every one of them is
    /// past its `until`. A row with one live lease and one ended lease is not
    /// an ended row, and saying otherwise would grey a Mac still being served.
    pub ended: bool,
}

impl PeerLsRow {
    /// Project one peers-file row, with the two ends the caller derived against
    /// its own single clock.
    ///
    /// The label is masked here rather than at the call site, so no caller can
    /// forget: this repository is public and this payload is read by a GUI.
    pub fn from_row(row: crate::peer::config::PeerRow, until: Option<u64>, ended: bool) -> Self {
        Self {
            node: row.node,
            label: masked_label(&row.label),
            endpoints: row.endpoints,
            added_at: row.added_at,
            allow: row.allow,
            lend: row.lend,
            sees_us_at: row.sees_us_at,
            until,
            ended,
        }
    }
}

/// One account's exit lock as `tcr peer ls --json` reports it.
///
/// Four keys because they are one answer: where this account leaves from,
/// whether an unavailable route refuses or falls back, whether the route is
/// available right now, and how long it has been unavailable. A panel that read
/// the pin without the liveness would draw "exits via <Mac>" over an account
/// whose requests are all failing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerExitJson {
    /// The file's own word: `local`, or `via <peer id>`.
    pub egress: String,
    /// Whether a pin that cannot be honoured refuses rather than falling back.
    pub egress_strict: bool,
    /// Whether the pinned Mac is unreachable from here right now.
    pub peer_down: bool,
    /// How long the pinned Mac has been unreachable, in seconds. Absent when
    /// this Mac has never recorded contact with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_seconds: Option<u64>,
}

/// One Mac heard on the LAN and not yet trusted, as `tcr peer ls --json`
/// shows it: [`crate::peer::state::Found`] projected for a reader, the found
/// row's twin of [`PeerLsRow`].
///
/// No `node`, on purpose: a found row carries no key to pin, because the
/// beacon it comes from carries none, and `PeerListDocument`'s Swift decoder
/// already reads that absence as "not trusted, not pinned, nothing to dial
/// without pressing Trust first".
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerFoundRow {
    /// The name it announced, already whitelisted on arrival and masked
    /// again on the way out. `None` when it announced no name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// What to dial to answer it: `host:port`, built by
    /// [`crate::peer::state::Found::dial_address`].
    pub address: String,
    /// Always `false`. Written explicitly rather than left to a decoder's
    /// default, so no reader depends on the coincidence that an object with
    /// no `node` would default to the same answer.
    pub trusted: bool,
    /// When it was last heard, Unix milliseconds.
    pub last_seen_ms: i64,
    /// Every source that heard it: `"lan"` (mDNS) and/or `"tailnet"`. When
    /// both did, [`Self::address`] is the tailnet one: see
    /// [`crate::peer::discovery::FoundList::shown`].
    pub sources: Vec<crate::peer::state::FoundSource>,
}

/// One entry in `tcr peer ls --json`'s `peers` array: a pinned Mac or a found
/// one. Untagged, so both variants serialize as flat objects into one array
/// and the Swift decoder needs no discriminator field, only whether `node`
/// is present.
///
/// The order within the array matters to a reader who wants "what does this
/// Mac know about right now" in one glance: trusted rows first, found rows
/// after, newest first within the found group. `src/main.rs` builds the
/// array in that order; this type does not enforce it, because ordering an
/// array is a property of how it was built, not of what its elements are.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PeerLsPeer {
    /// A pinned Mac.
    Trusted(PeerLsRow),
    /// A Mac heard announcing, not pinned.
    Found(PeerFoundRow),
}

/// The caps the panel's Advanced pane draws, so it and this binary cannot
/// disagree about what they are.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerCapsJson {
    pub found_rows: usize,
    pub found_per_address: usize,
    pub pending: usize,
    pub knock_interval_ms: i64,
    pub knock_burst: u32,
    pub unauthenticated_sockets: usize,
}

/// The whole `tcr peer ls --json` document.
///
/// The panel reads this. The count-and-rows pairs are both present on purpose:
/// a count alone makes the panel ask a second question to render the row, and
/// the two calls would see two different instants.
///
/// Serialize only: `LentTo` has no `Deserialize`, and a reader of this document
/// is the Swift panel, which has its own model. A test asserting the shape does
/// it against the JSON, which is the contract that crosses.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerLsJson {
    pub supported: bool,
    /// One entry per Mac this node knows about right now: pinned, or heard
    /// announcing and not yet trusted. See [`PeerLsPeer`], which is an
    /// explicit projection and carries no secret.
    pub peers: Vec<PeerLsPeer>,
    /// Macs asking to pair.
    ///
    /// Each row's `addr` is the address to ANSWER on, `host:port` when the
    /// knock said which port its listener is bound to and the bare host when
    /// it did not. The panel hands that string to `tcr peer pair`, so it is
    /// built by `crate::peer::state::Knock::dial_address` rather than being
    /// the bare key the file coalesces knocks under.
    pub pending: Vec<crate::peer::state::Knock>,
    pub pending_count: usize,
    /// Blocked addresses, with the key where one was learned.
    pub blocked: Vec<crate::peer::state::Ban>,
    pub blocked_count: usize,
    /// Muted addresses, quiet until the deadline lifts on its own.
    pub muted: Vec<crate::peer::state::Mute>,
    pub muted_count: usize,
    /// How many found rows this Mac is holding back past
    /// `crate::peer::discovery::MAX_FOUND_ROWS`, carried from the serving
    /// process's browse (`crate::peer::state::PeerState::found_not_shown`).
    /// Zero when nothing was held back, and also zero when no serving
    /// process is browsing at all: a down proxy holds nothing back, it
    /// simply holds nothing.
    pub limited: usize,
    pub caps: PeerCapsJson,
    /// The "Lent to …" line, per account label.
    pub lent_to: std::collections::BTreeMap<String, Vec<crate::peer::lease::LentTo>>,
    /// Whether this Mac may be reached from off its own LAN: the peers file's
    /// own setting, not a reachability test.
    pub internet: bool,
    /// Whether this Mac is on a network at all right now: at least one
    /// non-loopback interface up with a usable address. See
    /// [`network_fact::network_present`] for the rule. `false` is the fact
    /// that turns "Looking" into "No network" on the Peers tab; old readers
    /// that do not know this key yet simply ignore it.
    pub network: bool,
    /// Every account that has an exit lock, keyed by the label a `--scope`
    /// names it by.
    pub exits: std::collections::BTreeMap<String, PeerExitJson>,
    /// Why every field above reads empty when the peers file itself could
    /// not be parsed, rather than because nothing is pinned. Follows
    /// [`StatusPayload::peers_error`]'s shape, `None` on the ordinary path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peers_error: Option<String>,
}

impl PeerLsJson {
    /// The whole document when the peers file itself could not be read: every
    /// count and row is the honest "nothing to report" default, and
    /// [`Self::peers_error`] is the only field that says this is a read
    /// failure and not an empty mesh. `caps` still reports the real
    /// compile-time limits: they describe this build, not the file.
    pub fn unreadable(error: String) -> Self {
        Self {
            supported: true,
            peers: Vec::new(),
            pending: Vec::new(),
            pending_count: 0,
            blocked: Vec::new(),
            blocked_count: 0,
            muted: Vec::new(),
            muted_count: 0,
            limited: 0,
            caps: PeerCapsJson {
                found_rows: crate::peer::discovery::MAX_FOUND_ROWS,
                found_per_address: crate::peer::discovery::MAX_FOUND_PER_ADDRESS,
                pending: crate::peer::state::MAX_PENDING_KNOCKS,
                knock_interval_ms: crate::peer::listener::KNOCK_INTERVAL_MS,
                knock_burst: crate::peer::listener::KNOCK_BURST,
                unauthenticated_sockets: crate::peer::listener::MAX_UNAUTHENTICATED_SOCKETS,
            },
            lent_to: std::collections::BTreeMap::new(),
            internet: false,
            network: network_fact::network_present(),
            exits: std::collections::BTreeMap::new(),
            peers_error: Some(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A found row on `tcr peer ls --json`'s `peers` array has no `node`
    /// key, no `id` key, and `"trusted":false`, written explicitly.**
    ///
    /// `PeerListDocument`'s Swift decoder reads a `node`-less object as a
    /// found row and reads `trusted` as `false` by default when `node` is
    /// absent, but this project's own rule is that nothing here may depend
    /// on that coincidence. Watch it fail by wrapping `trusted` in
    /// `#[serde(skip_serializing_if)]` on [`PeerFoundRow`]: the key
    /// disappears from the wire and this assertion catches it.
    #[test]
    fn a_found_peer_serializes_with_no_node_key_and_trusted_written_explicitly() {
        let found = PeerLsPeer::Found(PeerFoundRow {
            name: Some("studio-mac".to_string()),
            address: "198.51.100.7:7755".to_string(),
            trusted: false,
            last_seen_ms: 12_000,
            sources: vec![crate::peer::state::FoundSource::Lan],
        });
        let value = serde_json::to_value(&found).expect("serialize a found row");
        let object = value.as_object().expect("a found row is an object");
        assert!(
            !object.contains_key("node"),
            "a found row must carry no node key: {object:?}"
        );
        assert!(
            !object.contains_key("id"),
            "a found row must carry no id key: {object:?}"
        );
        assert_eq!(
            object.get("trusted"),
            Some(&serde_json::Value::Bool(false)),
            "trusted must be written explicitly as false: {object:?}"
        );
    }

    /// **A found row carries `sources` as the contract spells it.**
    #[test]
    fn a_found_peer_lists_its_sources() {
        use crate::peer::state::FoundSource;
        let found = PeerFoundRow {
            name: None,
            address: "100.64.0.2:7755".to_string(),
            trusted: false,
            last_seen_ms: 1,
            sources: vec![FoundSource::Lan, FoundSource::Tailnet],
        };
        assert_eq!(
            serde_json::to_value(&found).expect("serialize")["sources"],
            serde_json::json!(["lan", "tailnet"])
        );
    }
}
