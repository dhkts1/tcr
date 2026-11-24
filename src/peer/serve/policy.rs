use axum::http::{HeaderMap, HeaderName};

// ONE list, two readers. See the module docs: these are `pub(crate)` in
// `src/proxy.rs` precisely so this file holds no list and no matcher of its own.
use crate::proxy::{
    CLIENT_CREDENTIAL_PREFIXES, CLIENT_TOKEN_REFRESH_PATH, is_request_hop_by_hop, path_is_under,
};

/// **The borrower's refusal**: may a SERVE be opened for this path at all?
///
/// `false` means fall through to the local path, never an error page, because
/// the request is perfectly servable here, just not there.
///
/// Reads `CLIENT_CREDENTIAL_PREFIXES` and `CLIENT_TOKEN_REFRESH_PATH` from
/// `src/proxy.rs` through `path_is_under`, and holds no list of its own.
///
/// Refuses every **Codex** path too, by the one classifier the request path
/// uses (`crate::provider::Provider::of_path`). Codex is not lent over the mesh
/// in this version: a lease is sized on the lender's Anthropic windows, and the
/// lender's picker would serve the request on a Codex account no lease accounts
/// for. Both halves ask this function, so the borrower's seam builds no `Ask`
/// for a Codex request and a lender closes on a frame that names one.
pub fn serve_is_allowed_for_path(path: &str) -> bool {
    // FIRST, and before any prefix compare below can mean anything: a path the
    // upstream URL parser would read differently than these compares do. See
    // [`relay_path_is_routable`], the review's H1, and note that it is asked
    // HERE rather than at each of this function's four call sites, so a fifth
    // caller cannot be the one that forgets.
    if !relay_path_is_routable(path) {
        return false;
    }
    if !crate::provider::Provider::of_path(path).on_peer_mesh() {
        return false;
    }
    if CLIENT_CREDENTIAL_PREFIXES
        .iter()
        .any(|base| path_is_under(path, base))
    {
        return false;
    }
    // This proxy's OWN local control surface, which is not an upstream path at
    // all. Behind it sit the local `/_tcr/` endpoints, including the one
    // privileged mutation that ADDS A LIVE CREDENTIAL, whose entire
    // authorization is `local_endpoint_gate` plus a content type
    // (`src/proxy.rs:1137`), that is, the fact that the caller reached
    // loopback. Relaying it would mean another host's request arriving at this
    // gate having satisfied it by construction. Matched with `path_is_under`,
    // the SAME matcher the proxy's own guard uses on this prefix
    // (`src/proxy.rs:2001`), so the two cannot disagree about what is local: a
    // spelling the proxy forwards upstream is a spelling this refuses to relay
    // for no reason, and a spelling the proxy answers locally is one a relay
    // must never reach.
    if path_is_under(path, crate::proxy::LOCAL_PREFIX) {
        return false;
    }
    // The seventh path, and the one a reader of the array above misses. Matched
    // the same way, so `/v1/oauth/token/` cannot fall through the way it once
    // fell through an exact compare.
    !path_is_under(path, CLIENT_TOKEN_REFRESH_PATH)
}

/// What a path [`serve_is_allowed_for_path`] refused IS, for the log line that
/// names the refusal, asked in that function's own order so the label names the
/// refusal that actually fired: an unroutable path ([`relay_path_is_routable`]),
/// refused first whatever else it is, because the upstream URL parser would read
/// it differently than it is spelled; then a Codex path, refused because Codex is
/// not lent over the mesh in this version; else a client-credential or
/// local-control path. Both refusal sites read it, the lender's backstop in
/// [`handle_serve_on`] and the borrower's `PeerLeaseProvider::try_serve`, so
/// neither blames a credential for the Codex refusal or for a path the parser
/// would have rewritten.
pub fn refused_path_kind(path: &str) -> &'static str {
    if !relay_path_is_routable(path) {
        "an unroutable path (one the upstream URL parser would read differently than it is \
         spelled)"
    } else if crate::provider::Provider::of_path(path).on_peer_mesh() {
        "a client-credential or local-control path"
    } else {
        "a Codex path (Codex is not lent over the mesh in this version)"
    }
}

/// **May a relayed path be built into a URL at all?** The review's H1, and the
/// check that has to answer before [`serve_is_allowed_for_path`] means anything.
///
/// Every refusal above is a PREFIX COMPARE on the raw frame string, and the
/// string is a `String` the borrower chose. The lender then built its own
/// request with `format!("{upstream}{path}")`, so what went on the wire was
/// whatever `Url::parse` made of the pair, and that parser normalizes per
/// WHATWG. Measured on this repository's pinned `url`, with `upstream =
/// "http://127.0.0.1:3456"`:
///
/// ```text
/// /x/../_tcr/accounts/control       -> 127.0.0.1:3456 /_tcr/accounts/control
/// @169.254.169.254/latest/meta-data -> 169.254.169.254 /latest/meta-data
/// /v1/code\foo                      -> 127.0.0.1:3456 /v1/code/foo
/// ```
///
/// So three things passed the refusals above and arrived somewhere else. The
/// first reached the lender's own privileged local route having satisfied
/// `local_endpoint_gate` BY CONSTRUCTION, the caller is loopback, the target is
/// loopback, no `Sec-Fetch-*` header survives [`LENDER_FORWARDED_HEADERS`], and
/// `content-type: application/json` does, which is the exact outcome
/// [`serve_is_allowed_for_path`]'s own comment says it prevents. The second is
/// an SSRF with the lender's network position, link-local metadata included, on
/// a path that does not begin with `/` and therefore turns the authority into
/// userinfo. The third normalizes onto a `CLIENT_CREDENTIAL_PREFIXES` path.
///
/// **This is not a new class of defect in this tree, it is the same one twice.**
/// `src/proxy.rs` records it as measured in the same words and fixed it for the
/// local client with [`crate::proxy::path_is_ambiguous`], applied on the
/// forwarding path. The SERVE path never called it. So this asks the SAME
/// function rather than re-deriving the parser's rules here, one matcher, two
/// readers, for the reason the module docs give for the path lists.
///
/// # The escaped separators, which are this path's own case
///
/// `crate::proxy::path_is_ambiguous` deliberately leaves `%5c` alone and says
/// why: the URL parser does not decode it, so for the LOCAL forwarding path the
/// escaped spelling means exactly what it says. A relay is the other case. The
/// string this function guards is handed to a SECOND server, this Mac's own
/// proxy, over loopback, and then Anthropic's, and a server that decodes
/// `%2F` to `/` before its own routing reads `/v1/messages/..%2F_tcr` as
/// `/v1/messages/../_tcr`, which is the dot-segment escape this whole function
/// exists to refuse, arriving one decode later. Nothing in this tree can
/// promise what every hop's decoder does, and no legitimate Anthropic client
/// emits an escaped separator, so the escaped spellings are refused here.
///
/// Refused HERE and not by widening the proxy's matcher, for the reason that
/// matcher gives: the proxy routes on `uri.path()`, which it has already
/// parsed, so widening it would refuse local requests for no gain.
///
/// Three checks, and the rooted one is first: a path that does not start with
/// `/` is not a path at all for this purpose, whatever else is true of it.
pub fn relay_path_is_routable(path: &str) -> bool {
    if !path.starts_with('/') {
        return false;
    }
    // Case-insensitively, because `%2f` and `%2F` are the same escape and a
    // check that reads one spelling is a check a borrower spells the other way.
    let lowered = path.to_ascii_lowercase();
    if lowered.contains("%2f") || lowered.contains("%5c") {
        return false;
    }
    // A protocol-relative `//host/...` is rooted AND names an authority: `/`
    // then `/` is an empty first segment, which `path_is_ambiguous` has no
    // reason to object to and `Url::parse` reads as a host. Refused here rather
    // than by widening the proxy's own matcher, because the proxy never sees
    // one: its own path comes out of `uri.path()`, which has already resolved
    // the authority.
    if path.starts_with("//") {
        return false;
    }
    !crate::proxy::path_is_ambiguous(path)
}

/// **The borrower's scrub**, applied to every frame before it is written.
///
/// Removes every name in `CLIENT_CREDENTIAL_HEADERS` and every hop-by-hop
/// request header the proxy already refuses to forward
/// (`is_request_hop_by_hop`, `src/proxy.rs:3698`). Returns what it removed, so
/// the caller can assert on it: the gate for this is a frame that contains none
/// of them, watched red by deleting the scrub.
///
/// The hop-by-hop half is asked of `src/proxy.rs` rather than re-listed here.
/// Two reasons, and the second is the load-bearing one: a SERVE frame is a
/// request leaving this host, so every name that is wrong on the upstream hop is
/// wrong here for the same reason; and `proxy-authorization` is **a credential**
/// as well as a hop-by-hop header, so a second list is a second place for a
/// credential name to go missing.
pub fn scrub_client_credentials(headers: &mut HeaderMap) -> usize {
    // `HeaderMap::remove` drops EVERY value under the name, not the first, so a
    // client that sent `authorization` twice leaves nothing behind. The count is
    // header NAMES removed, which is what the gate asserts on.
    let credentials = CLIENT_CREDENTIAL_HEADERS
        .iter()
        .filter(|name| headers.remove(**name).is_some())
        .count();

    let hop_by_hop: Vec<HeaderName> = headers
        .keys()
        .filter(|name| is_request_hop_by_hop(name.as_str()))
        .cloned()
        .collect();
    let hops = hop_by_hop
        .iter()
        .filter(|name| headers.remove(*name).is_some())
        .count();

    credentials + hops
}

/// The header names the borrower removes as CREDENTIALS, in one place because
/// both the scrub and anything that asserts on the scrub must read the same
/// list. The hop-by-hop names are not here: those come from `src/proxy.rs`, see
/// [`scrub_client_credentials`].
///
/// - `x-api-key` because an api-key client authenticates with it alone, so
///   scrubbing only the Bearer would send the other credential shape untouched.
/// - `proxy-authorization` because this proxy accepts one
///   (`ProxyConfig::api_key`), so it is a credential for THIS hop that a relay
///   would otherwise hand to another host. It is also hop-by-hop, which is
///   belt and braces rather than a duplicate: it must not survive either check.
/// - `cookie` because a session cookie is a bearer credential in every way that
///   matters and nothing upstream of a lease needs one.
/// - `x-tcr-key` ([`crate::request_key::HEADER`]) because it is this proxy's
///   request key, a credential for THIS hop only, like `proxy-authorization`.
pub const CLIENT_CREDENTIAL_HEADERS: [&str; 5] = [
    "authorization",
    "x-api-key",
    "proxy-authorization",
    "cookie",
    crate::request_key::HEADER,
];

/// The header names the LENDER copies from a relayed request onto the request
/// it makes through its own proxy. Everything else is dropped.
///
/// An allowlist, for the reason in the module docs: the borrower's scrub is the
/// enforcement point, this is the lender's backstop, and a backstop written as
/// a denylist forwards every name nobody thought of.
///
/// - `content-type` and `accept`, because the request is JSON or SSE and the
///   answer's shape follows from them.
/// - `anthropic-version` and `anthropic-beta`, because they select the API
///   contract the borrower's client is written against; substituting the
///   lender's would answer a shape the borrower cannot parse.
/// - `user-agent`, because Anthropic's own rate-limit and abuse signals read
///   it, and a relayed request that claimed to be something else would be a
///   worse citizen than one that says what it is.
///
/// Nothing else. `host` and `content-length` are the lender's HTTP client's to
/// set and are wrong if inherited; `authorization`, `cookie`, `x-api-key` and
/// `proxy-authorization` are credentials; anything unnamed is a header a
/// borrower invented.
pub const LENDER_FORWARDED_HEADERS: [&str; 5] = [
    "content-type",
    "accept",
    "anthropic-version",
    "anthropic-beta",
    "user-agent",
];

/// Whether the lender copies this header name onto its own request.
///
/// Case-insensitive, because a header name is case-insensitive on the wire and
/// a borrower writes the frame: `Cookie` and `cookie` are one name, and an
/// exact compare here would be an allowlist with a trivial bypass.
pub fn lender_forwards_header(name: &str) -> bool {
    LENDER_FORWARDED_HEADERS
        .iter()
        .any(|allowed| name.eq_ignore_ascii_case(allowed))
}

/// The one HTTP method a relayed request may carry.
///
/// [`ServeRequest::method`] used to be built as the literal `"POST"` and read
/// by nobody, so a borrower that sent `GET` had it silently `POSTed` on the
/// lender's account. The field is honoured instead of removed because the
/// borrower's client really does choose the method and a lender must be able to
/// say which one it will serve. See [`handle_serve_on`], which refuses
/// anything else rather than rewriting it.
pub const SERVE_METHOD: &str = "POST";
