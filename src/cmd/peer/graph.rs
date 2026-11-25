//! What the running proxy holds for the mesh: `tcr peer status`, and `tcr peer graph`, printed or served as a page.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use tcr::cli;
use tcr::config;
use tcr::peer;
use tcr::peer::status::graph::{GraphEdgeDetail, GraphRole, peer_graph_block};
use tcr::proxy;

use super::peer_cli;

/// `tcr peer status --json`: the peers block, off the RUNNING proxy.
///
/// # Why this is not `tcr peer ls`, and not `tcr status` either
///
/// `peer ls` projects the peers file and the state file, and answers with no
/// server at all. `tcr status --json` renders the ACCOUNTS array and has done
/// since long before the mesh existed; giving it a peers key would mean
/// turning an array into an object under every reader that already parses it.
/// This verb is the third question, what the process that is serving holds
/// for each pinned Mac, and it is the one the Peers tab's live half asks.
///
/// A proxy that is not running is a NON-ZERO exit and never an empty list. The
/// tab reads that as "keep the file half", which is the truth; an empty list
/// would say "you have no peers", which is not.
pub(super) async fn run_peer_status(args: peer_cli::PeerStatusArgs) -> anyhow::Result<()> {
    let config_path = args.config.unwrap_or_else(config::default_path);
    let config = config::load(&config_path)
        .with_context(|| format!("peer status: could not read {}", config_path.display()))?;
    let peers = cli::live_peers(&config)
        .await
        .map_err(|why| anyhow::anyhow!("peer status: {why}"))?;

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "peers": peers }))?
        );
        return Ok(());
    }
    if peers.is_empty() {
        println!("peer status: none pinned");
    }
    for row in &peers {
        println!(
            "peer status: peer={} name={} address={} last_seen_ms={} paths={} trusted={}",
            // The short form: this is a line a person reads. The full key is
            // `id` on the `--json` payload, which is what a panel joins on.
            row.display,
            row.name,
            row.address.as_deref().unwrap_or("none"),
            row.last_seen_ms
                .map_or_else(|| "never".to_string(), |at| at.to_string()),
            row.paths.len(),
            row.trusted
        );
    }
    Ok(())
}

/// `tcr peer graph`: one node per Mac, one edge per way to reach one, one edge
/// per live lease, as [`peer_graph_block`] derives it
/// off the peers file and the runtime-state file beside it.
///
/// Read-only and local, the same contract `tcr peer reach` already gives:
/// nothing here opens a socket to a peer or to the running proxy, so asking
/// for a picture of the mesh cannot change it. `--serve` is the one exception
/// that opens a socket at all, and it is a LISTENING one, on loopback only.
pub(super) async fn run_peer_graph(args: peer_cli::PeerGraphArgs) -> anyhow::Result<()> {
    let peers_path = args.peers.unwrap_or_else(peer::config::default_path);
    let config_dir = peers_path
        .parent()
        .map_or_else(peer::id::default_config_dir, Path::to_path_buf);
    let this = peer::id::NodeKey::load_or_mint(&config_dir)?.id();
    let this_label = peer::config::read_or_default(&peers_path)?.display_name();

    if args.serve {
        return serve_peer_graph(peers_path, this, this_label, &args.addr).await;
    }

    let graph = peer_graph_block(&peers_path, &this, &this_label, now_unix_ms());

    if args.json {
        println!("{}", serde_json::to_string_pretty(&graph)?);
        return Ok(());
    }

    println!(
        "peer graph: nodes={} edges={}",
        graph.nodes.len(),
        graph.edges.len()
    );
    for node in &graph.nodes {
        println!(
            "peer graph: node id={} name={} role={}",
            node.id,
            node.name,
            match node.role {
                GraphRole::ThisMac => "self",
                GraphRole::Peer => "peer",
            }
        );
    }
    for edge in &graph.edges {
        match &edge.detail {
            GraphEdgeDetail::Path {
                endpoint,
                path_kind,
                rtt_ms,
                loss_pct,
                ..
            } => println!(
                "peer graph: edge path from={} to={} endpoint={} kind={:?} rtt_ms={} loss_pct={}",
                edge.from,
                edge.to,
                endpoint,
                path_kind,
                rtt_ms.map_or("unmeasured".to_string(), |v| v.to_string()),
                loss_pct.map_or("unmeasured".to_string(), |v| v.to_string()),
            ),
            GraphEdgeDetail::Lease {
                lease_id, spent, ..
            } => println!(
                "peer graph: edge lease from={} to={} lease_id={} spent={:.2}",
                edge.from, edge.to, lease_id, spent
            ),
        }
    }
    Ok(())
}

/// Milliseconds since the unix epoch, the graph's own clock.
fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or_default(|since| since.as_millis() as i64)
}

/// `tcr peer graph --serve`: the inline HTML page from `GRAPH_PAGE_HTML`, and
/// the `/graph.json` it polls every 5 s, on ONE listener this verb binds
/// itself.
///
/// # Why loopback only
///
/// A graph carries every trusted Mac's address and, on a lease edge, its
/// spend, which nothing on this LAN needed to disclose to serve the mesh
/// before this verb existed. The mesh-served variant, any trusted Mac asking
/// any other's graph over the peer wire, is deferred; `docs/peers.md` says
/// so in its own paragraph. This verb binds ONLY an address whose
/// `ip().is_loopback()` is true and refuses to start on any other, so the
/// worst a operator can do by mistyping `--addr` is a refused start, never an
/// accidental LAN-wide disclosure surface.
async fn serve_peer_graph(
    peers_path: PathBuf,
    this: tcr_peer_wire::PeerId,
    this_label: String,
    addr: &str,
) -> anyhow::Result<()> {
    use axum::Router;
    use axum::extract::{Request, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::middleware::{self, Next};
    use axum::response::{Html, IntoResponse, Json, Response};
    use axum::routing::get;

    let socket: std::net::SocketAddr = addr
        .parse()
        .with_context(|| format!("peer graph --serve: {addr:?} is not a socket address"))?;
    if !socket.ip().is_loopback() {
        anyhow::bail!(
            "peer graph --serve: {addr:?} is not loopback; this page carries every trusted \
             Mac's address and lease spend, and this verb refuses to serve it off 127.0.0.1 \
             or ::1"
        );
    }

    /// Is `host` (a `Host` header value, `:port` and IPv6 brackets included)
    /// a name for THIS machine's loopback interface?
    ///
    /// The bind check above only proves the SOCKET is loopback; DNS rebinding
    /// answers from that same socket to a request whose `Host` names a public
    /// domain that resolves to `127.0.0.1` in the victim's browser only.
    ///
    /// **The answer is the proxy's, not a second copy of it.** This verb held
    /// its own transcription of that check for a while, and two
    /// spellings of "is this loopback" is two things to get wrong on a gate
    /// whose whole job is refusing a name. `proxy::host_is_loopback` takes a
    /// host with no port, which is what `proxy::strip_port` is for and what
    /// the transcription had inlined.
    fn host_is_loopback(host: &str) -> bool {
        proxy::host_is_loopback(proxy::strip_port(host))
    }

    /// The one check every route on this listener passes before its handler
    /// runs: the request's `Host` header must name a loopback address.
    /// Without it, a page open in a browser on this Mac can be pointed at
    /// `http://evil.example:18081/` (a DNS name the attacker controls,
    /// resolved to this loopback socket) and the bind check above would wave
    /// it through, because the bind check only ever asked what socket this
    /// server listens on, never what host the request claims to be for.
    async fn require_loopback_host(headers: HeaderMap, req: Request, next: Next) -> Response {
        let host = headers
            .get(axum::http::header::HOST)
            .and_then(|v| v.to_str().ok());
        match host {
            Some(host) if host_is_loopback(host) => next.run(req).await,
            _ => (
                StatusCode::FORBIDDEN,
                "peer graph --serve: this Host header does not name a loopback address",
            )
                .into_response(),
        }
    }

    #[derive(Clone)]
    struct GraphState {
        peers_path: PathBuf,
        this: tcr_peer_wire::PeerId,
        this_label: String,
    }

    async fn graph_json(State(state): State<GraphState>) -> impl IntoResponse {
        let graph = peer_graph_block(
            &state.peers_path,
            &state.this,
            &state.this_label,
            now_unix_ms(),
        );
        Json(graph)
    }

    async fn graph_page() -> impl IntoResponse {
        Html(GRAPH_PAGE_HTML)
    }

    let state = GraphState {
        peers_path,
        this,
        this_label,
    };
    let app = Router::new()
        .route("/", get(graph_page))
        .route("/graph.json", get(graph_json))
        .with_state(state)
        .layer(middleware::from_fn(require_loopback_host));

    let listener = tokio::net::TcpListener::bind(socket)
        .await
        .with_context(|| format!("peer graph --serve: could not bind {socket}"))?;
    println!("peer graph: serving on http://{socket}/ (loopback only, ctrl-c to stop)");
    axum::serve(listener, app)
        .await
        .context("peer graph --serve: the server stopped")?;
    Ok(())
}

/// The inline page `tcr peer graph --serve` returns at `/`: no build step, no
/// framework, one `<script>` that polls `/graph.json` every 5 s and redraws
/// an SVG from what it gets back. The colour rule mirrors
/// `PeerFormat.pathLine` on the Peers tab (`apps/macos/Sources/TcrBarCore/PeerFormat.swift`):
/// an unmeasured path is grey, `0` loss is "no loss", and a measured loss is
/// its own percentage; the three loss bands below are this page's own
/// addition, since a page has room for colour a text sub-line does not.
const GRAPH_PAGE_HTML: &str = include_str!("../../peer_graph_page.html");
