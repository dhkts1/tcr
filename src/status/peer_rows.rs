//! The status payload's peers block: one [`PeerStatusRow`] per pinned Mac, derived
//! read-only from the peers file and the peer state file.

use serde::{Deserialize, Serialize};

/// Which kind of route a [`PathStatus`] describes.
///
/// A typed discriminant rather than a bool or a string, because the panel
/// renders the two differently and a third kind (a relay, phase 6) is already
/// on the roadmap. `direct` and `via` on the wire, which is what the panel's
/// path sub-line decodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PathKind {
    /// A socket address this Mac dials itself.
    Direct,
    /// Reached through another pinned Mac that forwards for us.
    Via,
    /// Reached over a socket this Mac opened to another and parked there.
    ///
    /// Its own word rather than `via`, because the two differ in the fact a
    /// reader needs: a `via` path is one this Mac dials when it wants it, and
    /// a `reverse` one exists only while the far Mac keeps a socket parked, so
    /// "it stopped working" means different things and has different remedies.
    Reverse,
}

/// One way to reach one pinned Mac, and what is known about it.
///
/// # What is measured and what is honestly absent
///
/// [`Self::endpoint`] and [`Self::kind`] come off the peers file. Every other
/// field is a MEASUREMENT, and each one is `None` until something measures it
/// rather than `0`. The distinction is the whole reason this block exists, a
/// `0 ms` round trip beside a `0 %` loss reads as a perfect path, and that
/// sentence is exactly what a structurally-unwritten field used to print.
///
/// The two traffic fields have their writer now, the per-path meter, summed
/// into the state file's `pathTraffic` section, so for them the distinction is
/// live in both directions: an unmeasured path is `None` and a path that
/// carried nothing this hour is `Some(0)`. The per-endpoint round trip and the
/// loss fraction are the prober's and are still written by nobody into this
/// shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PathStatus {
    /// The socket address for a [`PathKind::Direct`] path, or the forwarding
    /// Mac's peer id for a [`PathKind::Via`] one.
    pub endpoint: String,
    pub kind: PathKind,
    /// Round trip over this path, milliseconds. `None` until the prober writes
    /// it; never `0`, which would read as instantaneous.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtt_ms: Option<f64>,
    /// Fraction of probes lost over this path, 0 to 1. `None` until the prober
    /// writes it; a measured zero is a real `0.0` and means something
    /// different.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loss_pct: Option<f64>,
    /// Bytes carried over THIS path in the last rolling hour, out of the state
    /// file's `pathTraffic` section
    /// ([`crate::peer::state::PathTraffic`]).
    ///
    /// The byte budget ([`crate::peer::tunnel::TunnelBudget`]) still charges a
    /// PEER and still cannot answer this: a cap keyed on anything but the peer
    /// whose handshake was checked is not a cap. The second meter beside it
    /// ([`crate::peer::tunnel::PathMeter`]) is keyed on the locator the traffic
    /// went over, and this is its figure.
    ///
    /// `None` and `Some(0)` are different answers and the distinction is the
    /// point: `None` is "nothing here measures this peer's paths", `Some(0)` is
    /// "measured, and this path carried nothing this hour". A path with no row
    /// of its own on a peer that HAS rows is a measured zero, which is what
    /// makes "it all went the other way" a readable fact rather than an
    /// absence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_per_hour: Option<u64>,
    /// Tokens drawn over THIS path in the last hour, from the same section and
    /// on the same `None`/`Some(0)` rule.
    ///
    /// It is a WINDOW and not a rate, which is what separates it from
    /// [`PeerStatusRow::tokens_per_hour`]: that one divides a lease's `spent`
    /// by the hours since it was granted and answers "how fast is this Mac
    /// drawing", this one sums what was actually debited inside the hour and
    /// answers "how much went this way". Two questions, and a reader that added
    /// the paths up expecting the row's figure would be adding a rate to a
    /// count.
    ///
    /// Zero on every path whose leases are measured in fractions of a window,
    /// for the reason [`crate::peer::lease::Ledger::note_lease_path`]'s
    /// neighbour gives: a fraction carries no token count, and this build
    /// refuses to relay a tokens-unit lease at all
    /// ([`tcr_peer_wire::LeaseRefusal::Unsupported`]), so a non-zero
    /// figure here needs that refusal lifted first.
    ///
    /// Which is to say: **this field is structurally zero in this build, on
    /// every path, and reading it as a measurement is reading a gap.** A
    /// tokens-unit lease cannot even be minted, because
    /// [`crate::peer::lease::clamp_to_grant`] takes its budget from
    /// `fraction_budget`, which is `None` for
    /// [`tcr_peer_wire::LeaseUnit::Tokens`] and answers `Unsupported` before a
    /// lease id is issued; and if one somehow existed,
    /// [`crate::peer::lease::Ledger::may_relay`] refuses it at the same
    /// predicate on every borrow. `Ledger::tokens_of`, the only caller that
    /// ever feeds [`crate::peer::tunnel::PathMeter::charge_tokens`], returns
    /// `None` for any other unit, so nothing reaches the meter.
    ///
    /// The hand-mode path does not fill the gap either, and it is the one that
    /// looks like it should: a borrower's `UsageHint` is applied by
    /// `Ledger::apply_usage_hint`, which raises the lease's `spent` and never
    /// touches the per-path meter, because that figure is a share of the
    /// owner's window and not a token count. So the live numbers today are
    /// [`Self::bytes_per_hour`] for both modes and `spent` on the lease row;
    /// a per-path token figure is a tokens lease unit away, which is a change
    /// to the mint, the relay check and the wire, not a display fix here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_per_hour: Option<u64>,
    /// Last time this Mac is known to have reached the peer over this path,
    /// Unix milliseconds.
    ///
    /// Derived, and deliberately narrow: `peer-state.json` records one
    /// `last_seen` PER PEER, so it can only be attributed to a path when the
    /// row has exactly one. With two endpoints and one timestamp, which path
    /// answered is unknown, and this stays `None` on both rather than claiming
    /// the same success twice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ok_ms: Option<i64>,
}

/// One pinned Mac as the SERVING process sees it: the sixteen fields the panel
/// already decodes (`TcrBarCore/PeerListDocument.swift`'s `PeerEntry`), plus
/// the per-path block and the one rate the ledger can answer.
///
/// # One vocabulary, and which side moved
///
/// The panel's field names win here, key for key (`lastSeenMs`, `leaseSpent`,
/// `byteCapPerHour`), and the Rust side does the renaming. The other direction
/// was tried by accident for three waves: the CLI emitted `node`, `label` and
/// `addrs` while the panel decoded `id`, `name` and `address`, every Swift
/// field is `decodeIfPresent`, and so the mismatch rendered an EMPTY row
/// instead of failing a test.
///
/// # Every field is either derived from a file or honestly absent
///
/// Nothing here probes, dials, or asks a peer anything: [`peer_status_rows`]
/// takes the two files' already-parsed contents and a clock. The fields that
/// need a live measurement ([`Self::in_flight`], [`Self::no_headroom`],
/// [`Self::bytes_per_hour`]) are `Option` and stay `None` on this path, so a
/// reader can tell "not measured" from "measured zero".
///
/// No `PartialEq`: [`crate::peer::config::LendGrant`] has none, and adding one
/// there is a change to another file. Tests compare the serialized JSON,
/// which is the contract that actually matters here anyway.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerStatusRow {
    /// The pinned key in its WIRE form, 52 Crockford characters, which is the
    /// same form `tcr peer ls --json` writes as a row's `node`.
    ///
    /// It used to be the `tcr-…` display form, and that made the two reads
    /// unjoinable: the panel merges the file's rows with the serving process's
    /// rows on this key ([`PeerListDocument.mergingLive`]), the halves carried
    /// two different spellings of the same Mac, and every live row was appended
    /// as a second row rather than merged. It is also the only form
    /// [`tcr_peer_wire::PeerId::parse`] accepts, so a command built from a row
    /// the panel is drawing (`--revoke`, `--relend`) now names something the
    /// CLI can read back.
    ///
    /// [`Self::display`] carries the short form for the screen.
    pub id: String,
    /// The same key in the `tcr-…` display form: about 50 bits, for a person to
    /// read one row aloud. Nothing parses it back.
    pub display: String,
    /// The operator's label, through the same sanitizer `tcr peer ls` uses,
    /// see [`masked_label`]. This repository is public and this payload is read
    /// by a GUI: a label that is an email or a uuid comes out `[masked]`.
    pub name: String,
    /// The newest endpoint, for the panel's single-address line. The full set
    /// is [`Self::paths`]; this is the one a row draws when it has no room for
    /// a sub-line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// Always true on this list: it is built from the peers FILE, and every row
    /// in that file is pinned. A found-not-trusted Mac is a discovery row and
    /// does not appear here at all.
    pub trusted: bool,
    /// Last time this peer answered, Unix milliseconds. Freshness, not
    /// liveness: a sleeping laptop is the normal state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_ms: Option<i64>,
    /// We may ask this Mac to carry us out ([`crate::peer::config::Allow::carry`]).
    pub carries: bool,
    /// This Mac may open a serve to us, which means it reads our requests in
    /// full ([`crate::peer::config::Allow::allow_disclose`]).
    pub serves: bool,
    /// Requests this Mac is serving right now. `None` here: in-flight lives in
    /// the running [`crate::peer::lease::Ledger`] and not in either file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_flight: Option<u32>,
    /// Fraction of the newest live lease's window already spent, 0 to 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_spent: Option<f64>,
    /// Seconds left on the newest live lease before the borrower must ask
    /// again. Clamped at zero, never negative.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_ttl_seconds: Option<i64>,
    /// Bytes carried for this Mac in the last rolling hour. `None` here for the
    /// same reason as [`PathStatus::bytes_per_hour`]: the budget lives in the
    /// serving process's [`crate::peer::tunnel::TunnelBudget`], not in a file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_per_hour: Option<u64>,
    /// The ceiling those bytes are measured against, the operator's own
    /// `maxTunnelBytesPerHour`, or
    /// [`crate::peer::tunnel::DEFAULT_MAX_TUNNEL_BYTES_PER_HOUR`], and only on
    /// a row this Mac actually carries for
    /// ([`crate::peer::config::Allow::gateway`]). A cap on a row that carries
    /// nothing is a meter with no meaning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub byte_cap_per_hour: Option<u64>,
    /// Tokens this Mac's leases have actually drawn per hour, from the lease
    /// ledger in `peer-state.json`.
    ///
    /// Derived, and only where the ledger can answer it: a lease measured in
    /// [`tcr_peer_wire::LeaseUnit::Tokens`] knows both its granted amount and
    /// the fraction spent, so `amount * spent` over the hours since it was
    /// granted is an OBSERVED rate. A lease measured as a utilization fraction
    /// carries no token count anywhere in this tree, so those rows are `None`
    /// rather than a fraction dressed up as tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_per_hour: Option<u64>,
    /// The lender says it has nothing spare. `None` on this path: it is a
    /// refusal that arrives on the wire, and neither file records one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_headroom: Option<bool>,
    /// The row-level end, Unix SECONDS: the soonest end among the
    /// live leases this row is party to while any is running, and the latest
    /// end once they have all ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<u64>,
    /// True only when this row has at least one lease and every one of them is
    /// past its end. A row with one live lease and one ended lease is not an
    /// ended row.
    pub ended: bool,
    /// The lender's own grants for this row, one per lease, each with its
    /// scope: the per-Mac sheet's "Lend from" list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lend: Vec<crate::peer::config::LendGrant>,
    /// Every way this Mac knows to reach that one, newest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<PathStatus>,
    /// Present when the newest endpoint is the tailnet address of a machine
    /// in the serving process's last fresh reading of the Tailscale app
    /// ([`crate::peer::state::PeerState::tailnet_route_of`]); absent
    /// otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tailnet: Option<PeerTailnetStatus>,
}

/// The `tailnet` object on a trusted row in the status payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerTailnetStatus {
    /// How the Tailscale app reaches that Mac, or `null` when it reports
    /// neither a direct path nor a relay.
    pub route: Option<crate::peer::state::TailnetRoute>,
}

/// A peer label, through the shared sanitizer, or `[masked]`.
///
/// The same rule `tcr peer ls` applies on the way out, and for the same reason:
/// this repository is public, the peers file is hand-editable JSON, and a label
/// that is an email, an org name or a uuid must not reach a screenshot, a log
/// or a bug report. A plain label is returned untouched.
pub fn masked_label(label: &str) -> String {
    tcr_peer_wire::sanitize_label(label).unwrap_or_else(|_| "[masked]".to_string())
}

/// Derive the status payload's peers block from the two files, read-only.
///
/// `rows` is the peers file's own list ([`crate::peer::config::PeerStore::peers`]),
/// `state` the peer state file ([`crate::peer::state::load`]), and `now_ms` the
/// clock every derived field is answered against, taken ONCE by the caller, so
/// two rows cannot disagree about whether the same 18:00 has arrived.
///
/// Nothing here opens a socket, and nothing here writes: a status request must
/// not be able to change the mesh, and the prober that WILL measure paths is a
/// separate writer feeding the same shape.
pub fn peer_status_rows(
    rows: &[crate::peer::config::PeerRow],
    state: &crate::peer::state::PeerState,
    now_ms: i64,
) -> Vec<PeerStatusRow> {
    let now_s = u64::try_from(now_ms.max(0) / 1_000).unwrap_or(0);
    rows.iter()
        .map(|row| {
            let last_seen_ms = state
                .last_seen
                .iter()
                .find(|(peer, _)| *peer == row.node)
                .map(|(_, at)| *at);
            let paths = peer_paths(row, last_seen_ms, state, now_ms);
            let (until, ended) = row_ends(row, state, now_s);
            let live = newest_live_lease(row, state, now_ms);
            // The newest endpoint, the one `address` below shows.
            let tailnet = row
                .endpoints
                .first()
                .and_then(|endpoint| match endpoint.locator {
                    crate::peer::config::Locator::Direct { addr } => {
                        state.tailnet_route_of(addr.ip(), now_ms)
                    }
                    crate::peer::config::Locator::Via { .. }
                    | crate::peer::config::Locator::Reverse { .. } => None,
                })
                .map(|route| PeerTailnetStatus { route });
            PeerStatusRow {
                id: row.node.to_wire(),
                display: row.node.display(),
                name: masked_label(&row.label),
                // NEWEST first, which is the order `paths` is built in, so the
                // one-line address and the sub-line's first entry cannot
                // disagree about which endpoint is current.
                address: paths.first().map(|path| path.endpoint.clone()),
                trusted: true,
                last_seen_ms,
                carries: row.allow.carry,
                serves: row.allow.allow_disclose,
                in_flight: None,
                lease_spent: live.map(|lease| lease.spent),
                lease_ttl_seconds: live
                    .map(|lease| ((lease.expires_at_ms - now_ms).max(0)) / 1_000),
                bytes_per_hour: None,
                byte_cap_per_hour: row
                    .allow
                    .gateway
                    .then_some(crate::peer::tunnel::DEFAULT_MAX_TUNNEL_BYTES_PER_HOUR),
                tokens_per_hour: tokens_per_hour(row, state, now_ms),
                no_headroom: None,
                until,
                ended,
                lend: row.lend.clone(),
                paths,
                tailnet,
            }
        })
        .collect()
}

/// The peers block for the SERVING process, read off the two files the mesh
/// keeps, at `now_ms`.
///
/// [`peer_status_rows`] is the derivation and takes already-parsed contents;
/// this is the one place that opens the files, so the proxy's status handler
/// has a single call and the derivation stays testable without a filesystem.
///
/// # Why an unreadable file is an empty block and not a 500
///
/// `tcr status` is how an operator finds out what this process thinks, very
/// much including when something is wrong with it. Refusing the whole payload
/// because the peers file is missing would take the accounts view away too,
/// and a missing peers file is the ORDINARY state of a Mac that has never
/// paired with anything. So a read that fails is logged at warn with the path
/// and the error, which is a surfaced error and not a swallowed one, and the
/// block is empty: no peers file, no peers. [`Self::peers_error`] on
/// [`StatusPayload`] is how that empty block is told apart from the ordinary
/// "no peers file at all" case; see [`peers_block_with_error`], which this
/// calls.
pub fn peers_block(peers_path: &std::path::Path, now_ms: i64) -> Vec<PeerStatusRow> {
    peers_block_with_error(peers_path, now_ms).0
}

/// [`peers_block`], plus the parse error when the peers file itself could not
/// be read, for [`StatusPayload::peers_error`]. A separate function rather
/// than changing `peers_block`'s own return type: every existing caller of
/// `peers_block` wants the rows only, and `payload.peers = peers_block(..)`
/// reads as what it does.
pub fn peers_block_with_error(
    peers_path: &std::path::Path,
    now_ms: i64,
) -> (Vec<PeerStatusRow>, Option<String>) {
    // `PeerStore` and not a bare `read_or_default`: it is the reader every
    // other surface goes through, and it is what derives each grant's `ended`
    // against the clock. A second reader here would be a second answer to
    // "has this lease ended".
    let store = match crate::peer::config::PeerStore::open(peers_path) {
        Ok(store) => store,
        Err(err) => {
            tracing::warn!(
                path = %peers_path.display(),
                error = %err,
                "status: the peers file could not be read, so the status payload's peers \
                 block is empty; the accounts it reports are unaffected"
            );
            return (Vec::new(), Some(err.to_string()));
        }
    };
    let state_path = crate::peer::state::peer_state_path(peers_path);
    let state = match crate::peer::state::load(&state_path, now_ms) {
        Ok(state) => state,
        Err(err) => {
            // A cold start rather than a refusal, the contract this file
            // already documents: it costs the rows their `lastSeen` and their
            // lease figures, and nothing else.
            tracing::warn!(
                path = %state_path.display(),
                error = %err,
                "status: no peer runtime state, so the peers block reports what the peers \
                 file alone can say"
            );
            crate::peer::state::PeerState::default()
        }
    };
    (peer_status_rows(&store.peers(), &state, now_ms), None)
}

/// The endpoints of one row as [`PathStatus`] values, newest first.
///
/// Both kinds are derived now. The row's own list carries the locator
/// ([`crate::peer::config::Locator`]), so a `Via` endpoint, a Mac that
/// forwards for us, comes through as [`PathKind::Via`] with the forwarding
/// Mac's peer id where a direct path carries a socket address. An earlier
/// version of this comment said every path was direct because the peers file
/// "holds socket addresses and nothing else"; that stopped being true when the
/// row started holding endpoints.
///
/// `observed_at_ms` is deliberately NOT copied onto the path. It says when this
/// Mac last LEARNED the endpoint, which is a different fact from when it last
/// reached the peer over it, and [`PathStatus::last_ok_ms`] is the second one.
pub(super) fn peer_paths(
    row: &crate::peer::config::PeerRow,
    last_seen_ms: Option<i64>,
    state: &crate::peer::state::PeerState,
    now_ms: i64,
) -> Vec<PathStatus> {
    use crate::peer::config::Locator;

    // Whether this peer's paths are measured AT ALL, decided once for the row:
    // a peer with no traffic row has never been metered and every one of its
    // paths answers `None`, a peer with any row has, and a path of its own with
    // no row carried nothing. Deciding it per path instead would make the two
    // answers depend on which endpoint happened to be looked at first.
    let metered = state
        .path_traffic
        .iter()
        .any(|traffic| traffic.peer == row.node);

    // One endpoint means one candidate for the peer-level `last_seen`; two mean
    // the timestamp cannot be attributed and both paths say so. See
    // `PathStatus::last_ok_ms`.
    let attributable = row.endpoints.len() == 1;
    row.endpoints
        .iter()
        .map(|endpoint| {
            // The wire id, not `display()`: `PeerStatusRow::id` (the key the
            // panel resolves names by, `mergingLive` and `peerNames` both key
            // on it) is the wire form, and a `Via`/`Reverse` endpoint that
            // carried the display form instead could never be looked up in
            // that map, so the tab drew the raw id where a name belonged.
            let (address, kind) = match endpoint.locator {
                Locator::Direct { addr } => (addr.to_string(), PathKind::Direct),
                Locator::Via { node } => (node.to_wire(), PathKind::Via),
                Locator::Reverse { node } => (node.to_wire(), PathKind::Reverse),
            };
            let traffic = state
                .path_traffic
                .iter()
                .find(|traffic| traffic.peer == row.node && traffic.locator == endpoint.locator);
            let measured = state
                .paths
                .iter()
                .find(|stat| stat.peer == row.node && stat.locator == endpoint.locator);
            PathStatus {
                endpoint: address,
                kind,
                rtt_ms: measured.and_then(|stat| stat.rtt_ms),
                loss_pct: measured.map(|stat| stat.loss_pct),
                bytes_per_hour: metered.then(|| {
                    traffic.map_or(0, |row| {
                        rolled(row.bytes_last_hour, row.updated_at_ms, now_ms)
                    })
                }),
                tokens_per_hour: metered.then(|| {
                    traffic.map_or(0, |row| {
                        rolled(row.tokens_last_hour, row.updated_at_ms, now_ms)
                    })
                }),
                last_ok_ms: attributable.then_some(last_seen_ms).flatten(),
            }
        })
        .collect()
}

/// One written total, read back against the clock: the figure it stood at, or
/// `0` once the hour it described has entirely rolled off.
///
/// A [`crate::peer::state::PathTraffic`] row is a SUM taken at an instant, not
/// the window itself, so the only two honest readings of it are "this is what
/// the hour held then" and "then is longer ago than the window, so it holds
/// nothing now". A partial roll-off is not derivable from a total and is not
/// attempted: the meter's next write is what moves the figure, and a status
/// reader that scaled it by elapsed time would be inventing a decay the traffic
/// never had.
///
/// The boundary is [`crate::peer::tunnel::TunnelBudget::trim`]'s, to the
/// millisecond: a row exactly one window old is still inside its window on both
/// sides, so "may carry this hour" and "carried this hour" cannot be read
/// against two different hours.
const fn rolled(total: u64, updated_at_ms: i64, now_ms: i64) -> u64 {
    if now_ms.saturating_sub(updated_at_ms) > crate::peer::tunnel::BUDGET_WINDOW_MS {
        0
    } else {
        total
    }
}

/// The newest lease this row is party to that has NOT expired, in either
/// direction: one the ledger minted for it, or one it granted this Mac.
///
/// Newest by `granted_at_ms`, because a row can hold several at once (decision
/// 12) and the panel draws one meter.
fn newest_live_lease<'a>(
    row: &crate::peer::config::PeerRow,
    state: &'a crate::peer::state::PeerState,
    now_ms: i64,
) -> Option<&'a tcr_peer_wire::Lease> {
    state
        .leases
        .iter()
        .filter(|held| held.peer == row.node)
        .map(|held| &held.lease)
        .chain(
            state
                .borrowed
                .iter()
                .filter(|borrowed| borrowed.lender == row.node)
                .map(|borrowed| &borrowed.lease),
        )
        .filter(|lease| lease.expires_at_ms > now_ms)
        .max_by_key(|lease| lease.granted_at_ms)
}

/// Observed tokens per hour for one row, from the lease ledger.
///
/// Summed over every live lease of that peer measured in
/// [`tcr_peer_wire::LeaseUnit::Tokens`]: `amount * spent` is what has actually
/// been drawn, and the hours since `granted_at_ms` is how long it took. A lease
/// granted less than a second ago is not a rate yet and contributes nothing,
/// dividing by that window would report a number in the billions.
///
/// `None`, not `Some(0)`, in BOTH cases where there is no rate to report: when
/// no lease of this row is measured in tokens at all (nothing in this tree
/// converts a utilization fraction to a token count), and when the only token
/// leases are younger than that first window. A zero would read as "this Mac
/// drew nothing", which is a claim and not an absence.
fn tokens_per_hour(
    row: &crate::peer::config::PeerRow,
    state: &crate::peer::state::PeerState,
    now_ms: i64,
) -> Option<u64> {
    /// A rate over a shorter window than this is noise, not a measurement.
    const MIN_ELAPSED_MS: i64 = 1_000;

    let mut total = 0.0_f64;
    let mut measured = false;
    for held in state.leases.iter().filter(|held| held.peer == row.node) {
        let tcr_peer_wire::LeaseUnit::Tokens(amount) = held.lease.unit else {
            continue;
        };
        let elapsed_ms = now_ms - held.lease.granted_at_ms;
        if elapsed_ms < MIN_ELAPSED_MS {
            // NOT measured. This sat above the check and marked the row
            // measured first, so a lease minted in the last second reported
            // `Some(0)`, "this Mac drew nothing", for a lease that had simply
            // not run long enough to divide by. The two answers this function
            // must keep apart are "no token lease here" and "not a rate yet",
            // and both of them are `None`; only a lease that has actually run
            // can contribute a number.
            continue;
        }
        measured = true;
        let hours = elapsed_ms as f64 / 3_600_000.0;
        // `spent` is a fraction of the granted amount, clamped: a ledger row
        // written by a newer build could carry a value outside 0..=1, and a
        // rate above what was granted is not a thing this can report. Dropping
        // the factor entirely reports the GRANT as if it had all been drawn,
        // which is the defect `a_lent_lease_in_tokens_reports_a_rate_from_the_ledger`
        // watched fail: 1 000 000 granted and half spent read as 500 000/h.
        total += (amount as f64) * held.lease.spent.clamp(0.0, 1.0) / hours;
    }
    measured.then(|| total.round().max(0.0) as u64)
}

/// The row-level `until` and `ended`, over every lease this row is
/// party to: the operator's grants, the leases the ledger minted against them,
/// and the leases that Mac has granted THIS one.
///
/// The same rule `tcr peer ls --json` answers with (`peer_ls_ends` in
/// `src/main.rs`), and the two must not drift: one row drawn by the panel from
/// this payload and the same row drawn from `peer ls` disagreeing about whether
/// a lease has ended is the exact confusion the two keys settled. Collapsing
/// them onto this one writer needs an edit in `src/main.rs`, which has not
/// happened yet.
fn row_ends(
    row: &crate::peer::config::PeerRow,
    state: &crate::peer::state::PeerState,
    now_s: u64,
) -> (Option<u64>, bool) {
    let mut ends: Vec<(Option<u64>, bool)> = row
        .lend
        .iter()
        .map(|grant| (grant.until, grant.has_ended(now_s)))
        .collect();
    ends.extend(
        state
            .leases
            .iter()
            .filter(|held| held.peer == row.node)
            .map(|held| (held.lease.until, lease_has_ended(&held.lease, now_s))),
    );
    ends.extend(
        state
            .borrowed
            .iter()
            .filter(|borrowed| borrowed.lender == row.node)
            .map(|borrowed| {
                (
                    borrowed.lease.until,
                    lease_has_ended(&borrowed.lease, now_s),
                )
            }),
    );

    if ends.is_empty() {
        return (None, false);
    }
    let all_ended = ends.iter().all(|(_, past)| *past);
    let named: Vec<u64> = ends.iter().filter_map(|(end, _)| *end).collect();
    let until = if all_ended {
        named.iter().copied().max()
    } else {
        // The SOONEST end among the live ones: that is the instant the row's
        // "ends in 1 h" counts down to. A lease with no end contributes no
        // candidate rather than an infinite one.
        ends.iter()
            .filter(|(_, past)| !*past)
            .filter_map(|(end, _)| *end)
            .min()
    };
    (until, all_ended)
}

/// Whether one wire lease's own end has passed, on the same clock.
///
/// A lease with no `until` has not ended by the clock: only its expiry ends it,
/// and that is a different field with a different meaning.
fn lease_has_ended(lease: &tcr_peer_wire::Lease, now_s: u64) -> bool {
    lease.until.is_some_and(|end| now_s >= end)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A trusted row gains `tailnet: {route}` exactly when its newest
    /// endpoint is the tailnet address of a machine in the server's last
    /// fresh reading**, and has no `tailnet` key otherwise.
    ///
    /// Watch it fail by building `tailnet` from `row.endpoints.last()`
    /// instead of `.first()`: the tailnet row then reads its older LAN
    /// endpoint and loses the key.
    #[test]
    fn a_trusted_row_on_a_tailnet_address_carries_its_route() {
        use crate::peer::config::{Allow, Endpoint, EndpointSource, PeerRow, TailnetMode};
        use crate::peer::state::{
            PeerState, TailnetPeer, TailnetRecord, TailnetRoute, TailnetState,
        };

        let row = |byte: u8, endpoints: &[&str]| PeerRow {
            node: tcr_peer_wire::PeerId([byte; 32]),
            label: format!("laptop-{byte}"),
            endpoints: endpoints
                .iter()
                .map(|addr| {
                    Endpoint::direct(addr.parse().expect("literal"), 1, EndpointSource::Hello)
                })
                .collect(),
            added_at: 1,
            allow: Allow::default(),
            lend: Vec::new(),
            rendezvous_secret: None,
            sees_us_at: None,
        };
        let rows = [
            row(2, &["100.64.0.2:7755", "192.168.1.20:7755"]),
            row(3, &["192.168.1.30:7755", "100.64.0.3:7755"]),
        ];
        let state = PeerState {
            tailnet: Some(TailnetRecord {
                mode: TailnetMode::App.into(),
                state: TailnetState::Ready,
                address: Some(std::net::Ipv4Addr::new(100, 64, 0, 1)),
                recorded_at_ms: 1_000,
                ..TailnetRecord::default()
            }),
            tailnet_peers: vec![
                TailnetPeer {
                    addresses: vec!["100.64.0.2".parse().expect("literal")],
                    route: Some(TailnetRoute::Relayed),
                },
                TailnetPeer {
                    addresses: vec!["100.64.0.3".parse().expect("literal")],
                    route: None,
                },
            ],
            ..Default::default()
        };
        let out = serde_json::to_value(peer_status_rows(&rows, &state, 2_000)).expect("serialize");
        assert_eq!(out[0]["tailnet"], serde_json::json!({"route": "relayed"}));
        assert!(
            out[1].get("tailnet").is_none(),
            "the newest endpoint is a LAN one: {}",
            out[1]
        );

        let stale = serde_json::to_value(peer_status_rows(
            &rows,
            &state,
            1_001 + crate::peer::state::TAILNET_RECORD_STALE_MS,
        ))
        .expect("serialize");
        assert!(
            stale[0].get("tailnet").is_none(),
            "a stale reading puts no pill on a row"
        );
    }
}
