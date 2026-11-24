//! The whole mesh as this Mac sees it, `tcr peer graph`'s document, built on the same
//! path derivation as the peers block.

use serde::{Deserialize, Serialize};

use super::peer_rows::{PathKind, masked_label, peer_paths};

/// [`peer_graph`], derived off the two files on disk rather than values a
/// caller already holds: the same split [`peers_block`] draws from
/// [`peer_status_rows`], for the same reason. `tcr peer graph` and the CLI's
/// own tests both need the derivation, and only one of them should know where
/// the state file lives.
///
/// Nothing here opens a socket: an unreadable peers file reports a graph with
/// only `this` as a node, never a refusal, so asking for a picture of a mesh
/// this Mac has not yet joined is not an error.
pub fn peer_graph_block(
    peers_path: &std::path::Path,
    this: &tcr_peer_wire::PeerId,
    this_label: &str,
    now_ms: i64,
) -> PeerGraph {
    let rows = match crate::peer::config::PeerStore::open(peers_path) {
        Ok(store) => store.peers(),
        Err(err) => {
            tracing::warn!(
                path = %peers_path.display(),
                error = %err,
                "status: the peers file could not be read, so the graph carries only this Mac"
            );
            Vec::new()
        }
    };
    let state_path = crate::peer::serve::peer_state_path(peers_path);
    let state = match crate::peer::state::load(&state_path, now_ms) {
        Ok(state) => state,
        Err(err) => {
            tracing::warn!(
                path = %state_path.display(),
                error = %err,
                "status: no peer runtime state, so the graph carries no lease or freshness edges"
            );
            crate::peer::state::PeerState::default()
        }
    };
    peer_graph(this, this_label, &rows, &state, now_ms)
}

/// The whole mesh as THIS Mac sees it: one node per Mac, one edge per way to
/// reach one, one edge per live lease.
///
/// # What a graph is for, and why it is not the peers block
///
/// [`PeerStatusRow`] answers "what is my link to that Mac", one row at a time,
/// in the panel's vocabulary. A graph answers a question no row can: which Macs
/// are reachable only THROUGH another one, and which way the borrowing flows.
/// Both are derived from the same two files by the same clock, so they cannot
/// disagree: the graph is a second projection, never a second source.
///
/// # Its own kind, and why it is not [`STATUS_KIND`]
///
/// A graph is a different document with a different shape, so it carries its
/// own discriminator: a reader handed this where a status payload was expected
/// must refuse it, and the version this shape moves at has nothing to do with
/// the status payload's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerGraph {
    /// Always [`PEER_GRAPH_KIND`]. Checked by the reader before it trusts a
    /// body, for the reason [`STATUS_KIND`] gives at length.
    pub kind: String,
    /// The clock every edge was derived against, Unix milliseconds. Taken ONCE
    /// so two edges cannot disagree about whether the same lease has ended.
    pub generated_at_ms: i64,
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
}

/// The discriminator on every [`PeerGraph`].
pub const PEER_GRAPH_KIND: &str = "tcr.peer.graph.v1";

/// Whether a node is the Mac that answered, or one it has pinned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GraphRole {
    /// The Mac this graph was asked of. Exactly one node carries it.
    #[serde(rename = "self")]
    ThisMac,
    /// A pinned Mac. One per row in the peers file, and nothing else: a found
    /// but untrusted Mac has no identity yet (it is learned in the handshake),
    /// so it cannot be a node in an identity-addressed graph.
    #[serde(rename = "peer")]
    Peer,
}

/// One Mac.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphNode {
    /// The pinned key in its `tcr-…` display form, which is the only name a
    /// Mac has here: an address is a locator and belongs on an edge.
    pub id: String,
    /// The operator's label through [`masked_label`], this graph can be served
    /// as a page, so a label that is an email or a uuid comes out `[masked]`.
    pub name: String,
    pub role: GraphRole,
    /// Last time that Mac answered, Unix milliseconds. Absent on the node that
    /// answered (it does not see itself) and on a Mac never yet reached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_ms: Option<i64>,
}

/// One directed fact between two Macs.
///
/// The common half is `from` and `to`; what KIND of fact it is, and the fields
/// that only that kind has, are [`GraphEdgeDetail`], one tagged value rather
/// than an edge carrying every field of both kinds with half of them null. A
/// reader that matches on `kind` gets exactly the fields that kind defines.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphEdge {
    /// The node the fact points FROM, as a [`GraphNode::id`].
    pub from: String,
    pub to: String,
    #[serde(flatten)]
    pub detail: GraphEdgeDetail,
}

/// Which kind of fact an edge is, and the fields only that kind has.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
// `rename_all` on an enum renames the VARIANTS, never the fields inside
// them, so it alone let `path_kind` and `expires_at_ms` onto the wire while
// the variant tags looked perfectly right. `rename_all_fields` is the half
// that reaches the fields, and the schema test is what caught the absence.
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum GraphEdgeDetail {
    /// A way to reach the Mac at `to`. Every figure is the prober's and absent
    /// until it lands, never a zero, which would read as a perfect path.
    Path {
        /// The socket address dialled, or the forwarding Mac's id on a
        /// [`PathKind::Via`] edge.
        endpoint: String,
        path_kind: PathKind,
        /// The Mac the bytes pass THROUGH, on a via edge only. It is what makes
        /// the graph a graph: the reader can see that reaching `to` costs a
        /// third Mac's willingness to forward.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        through: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rtt_ms: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        loss_pct: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_ok_ms: Option<i64>,
    },
    /// A live lease. `from` is the LENDER and `to` is the borrower, always, so
    /// the arrow points the way the quota flows and a reader never has to
    /// consult a direction field to know which Mac is paying.
    Lease {
        /// The ledger's own id, in the 32-character hex form every CLI surface
        /// prints and `--revoke` reads back
        /// ([`crate::peer::config::lease_id_string`]). Zero-padded, because
        /// unpadded is a DIFFERENT string for any id with a leading zero
        /// nibble, and a reader lining this edge up against a lease list or
        /// building a command from it would match nothing.
        lease_id: String,
        /// Fraction of the window drawn so far, 0 to 1.
        spent: f64,
        /// The renewal deadline, Unix milliseconds: the lease dies here unless
        /// it is renewed.
        expires_at_ms: i64,
        /// The operator's own "lend until", Unix SECONDS, when one was set. A
        /// different clock from `expires_at_ms` and deliberately kept apart.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        until: Option<u64>,
    },
}

/// Derive [`PeerGraph`] from the two files, read-only.
///
/// `this` and `this_label` are the answering Mac's own identity: it is not in
/// the peers file, which holds every Mac EXCEPT this one, so a graph that
/// derived its nodes from the file alone would have no centre and no edges.
///
/// Nothing here opens a socket or writes anything, the same contract
/// [`peer_status_rows`] carries: asking for a picture of the mesh must not be
/// able to change it.
pub fn peer_graph(
    this: &tcr_peer_wire::PeerId,
    this_label: &str,
    rows: &[crate::peer::config::PeerRow],
    state: &crate::peer::state::PeerState,
    now_ms: i64,
) -> PeerGraph {
    let me = this.display();
    let mut nodes = vec![GraphNode {
        id: me.clone(),
        name: masked_label(this_label),
        role: GraphRole::ThisMac,
        last_seen_ms: None,
    }];
    let mut edges: Vec<GraphEdge> = Vec::new();

    for row in rows {
        let peer = row.node.display();
        let last_seen_ms = state
            .last_seen
            .iter()
            .find(|(node, _)| *node == row.node)
            .map(|(_, at)| *at);
        nodes.push(GraphNode {
            id: peer.clone(),
            name: masked_label(&row.label),
            role: GraphRole::Peer,
            last_seen_ms,
        });

        // The paths, from the one derivation the peers block uses too, so a
        // path drawn on the graph and the same path drawn on the tab cannot
        // report different figures.
        for path in peer_paths(row, last_seen_ms, state, now_ms) {
            // `path.endpoint` is now the WIRE id on a Via/Reverse path (the
            // fix `PeerStatusRow`'s own endpoint needed, so the panel can
            // resolve it against `id`). This graph's node ids are all the
            // DISPLAY form instead (`me`, `peer`, above), and nothing here
            // re-resolves `through` against `nodes`, so carrying the wire id
            // through unchanged would put one id space in `to` and a
            // different one in `through` for the same Mac. Parsed back to
            // display for this edge alone; a parse failure (an id this build
            // cannot read) falls back to the wire form rather than losing
            // the field.
            let endpoint = if matches!(path.kind, PathKind::Via | PathKind::Reverse) {
                tcr_peer_wire::PeerId::parse(&path.endpoint)
                    .map_or_else(|_| path.endpoint.clone(), |id| id.display())
            } else {
                path.endpoint.clone()
            };
            let through = matches!(path.kind, PathKind::Via).then(|| endpoint.clone());
            edges.push(GraphEdge {
                from: me.clone(),
                to: peer.clone(),
                detail: GraphEdgeDetail::Path {
                    endpoint,
                    path_kind: path.kind,
                    through,
                    rtt_ms: path.rtt_ms,
                    loss_pct: path.loss_pct,
                    last_ok_ms: path.last_ok_ms,
                },
            });
        }

        // Leases this Mac has LENT that peer: this Mac's quota, drawn there.
        for held in state.leases.iter().filter(|held| held.peer == row.node) {
            if held.lease.expires_at_ms <= now_ms {
                continue;
            }
            edges.push(lease_edge(&me, &peer, &held.lease));
        }
        // And the ones it has BORROWED from that peer: the arrow turns around,
        // because the lender is always `from`.
        for borrowed in state
            .borrowed
            .iter()
            .filter(|borrowed| borrowed.lender == row.node)
        {
            if borrowed.lease.expires_at_ms <= now_ms {
                continue;
            }
            edges.push(lease_edge(&peer, &me, &borrowed.lease));
        }
    }

    PeerGraph {
        kind: PEER_GRAPH_KIND.to_string(),
        generated_at_ms: now_ms,
        nodes,
        edges,
    }
}

/// One lease edge, lender to borrower.
fn lease_edge(from: &str, to: &str, lease: &tcr_peer_wire::Lease) -> GraphEdge {
    GraphEdge {
        from: from.to_string(),
        to: to.to_string(),
        detail: GraphEdgeDetail::Lease {
            lease_id: crate::peer::config::lease_id_string(lease.lease_id),
            spent: lease.spent,
            expires_at_ms: lease.expires_at_ms,
            until: lease.until,
        },
    }
}
