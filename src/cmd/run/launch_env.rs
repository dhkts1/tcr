//! The environment `tcr run` starts the agent in: the proxy URL and launch key, the custom headers, and the see-through CA variables.

use std::path::Path;

use tcr::proxy::GROUP_HEADER_NAME;
use tcr::proxy::live_client;
use tcr::{request_key, status};

/// `--group`'s header-composition half (steps c/d): the new value for
/// `ANTHROPIC_CUSTOM_HEADERS`, merging with whatever this process already
/// inherited rather than clobbering it — a user may have set their own
/// headers, and silently dropping them is a bug.
///
///  - No inherited value (or an empty one) → just `x-tcr-group: <name>`.
///  - Inherited value present → append ours on a new line.
///  - Inherited value already carries an `x-tcr-group` line (matched
///    case-insensitively on the header NAME) → that line is replaced, not
///    duplicated. Not hypothetical: `tcr run` nests (see `mark_run_active` /
///    [`RUN_ACTIVE_ENV`]), and two conflicting group headers on one request
///    is a worse failure than either value alone.
///  - Every OTHER inherited line's text and order is preserved exactly.
///
/// Pure — takes the inherited value and the group name, returns the new
/// value — so all of the above is testable without spawning a process.
pub(super) fn compose_group_header(inherited: Option<&str>, group: &str) -> String {
    compose_custom_header(inherited, GROUP_HEADER_NAME, group)
}

/// [`compose_group_header`]'s rule for any one header `name`: `inherited` with
/// every `name` line (case-insensitive) removed, the others kept in order, and
/// `name: value` appended last. The request key's header goes through it too
/// ([`apply_base_url_env`]), so a nested `tcr run` replaces the outer key line
/// rather than sending two.
fn compose_custom_header(inherited: Option<&str>, name: &str, value: &str) -> String {
    let ours = format!("{name}: {value}");
    let Some(inherited) = inherited.filter(|s| !s.is_empty()) else {
        return ours;
    };
    let mut lines: Vec<&str> = inherited
        .lines()
        .filter(|line| {
            let line_name = line.split(':').next().unwrap_or(line).trim();
            !line_name.eq_ignore_ascii_case(name)
        })
        .collect();
    lines.push(&ours);
    lines.join("\n")
}

/// A request key ([`tcr::request_key`]) that `tcr run` can write as it is into a
/// proxy URL's password and into an `ANTHROPIC_CUSTOM_HEADERS` line: letters,
/// digits, `-`, `.`, `_` and `~` only, the characters a URL's userinfo carries
/// unescaped. The server's own keys are hex. Built only by [`LaunchKey::decide`],
/// and its `Debug` never prints the value, so a `{:?}` cannot leak it.
#[derive(Clone, PartialEq, Eq)]
pub(super) struct LaunchRequestKey(String);

impl std::fmt::Debug for LaunchRequestKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LaunchRequestKey(***)")
    }
}

impl LaunchRequestKey {
    /// `None` for an empty key, or one holding any other character.
    pub(super) fn parse(raw: &str) -> Option<Self> {
        (!raw.is_empty()
            && raw
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')))
        .then(|| Self(raw.to_string()))
    }

    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

/// Whether this launch carries the request key: decided ONCE per launch, from the
/// config's `proxy.requestKey` and one read of the running server's status, and
/// read by every env writer ([`apply_see_through_env`],
/// [`apply_codex_see_through_env`], [`apply_codex_launch`], [`apply_base_url_env`]).
///
/// The key is sent only to a server whose status reports `requestKeyMode`. An
/// older server compares the WHOLE `Proxy-Authorization` header against the bare
/// Codex marker, so a keyed `tcr-codex:<key>` URL would silently run unpooled
/// there. Every other case launches exactly as before keys existed, and
/// [`Self::notice`] says why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum LaunchKey {
    /// Send this key.
    Send(LaunchRequestKey),
    /// The config holds no `proxy.requestKey` (the server writes one at boot).
    NoKeyInConfig,
    /// The config's key holds a character a proxy URL cannot carry as written.
    KeyNotUrlSafe,
    /// The running server's status could not be read; the reason.
    StatusUnread(String),
    /// The running server answered without `requestKeyMode`: it predates
    /// request keys, or holds none.
    ServerPredatesKeys,
}

impl LaunchKey {
    /// `config_key` is the loaded config's `proxy.requestKey`; `status` is the
    /// launch's one read of the running server ([`live_client::fetch_live_status`]).
    /// `tcr run` never writes a key: only the server generates one.
    pub(super) fn decide(
        config_key: Option<&str>,
        status: &Result<status::StatusPayload, live_client::LiveStatusError>,
    ) -> Self {
        let Some(raw) = config_key.filter(|key| !key.is_empty()) else {
            return Self::NoKeyInConfig;
        };
        let Some(key) = LaunchRequestKey::parse(raw) else {
            return Self::KeyNotUrlSafe;
        };
        match status {
            Err(err) => Self::StatusUnread(err.why()),
            Ok(payload) if !payload.understands_request_keys() => Self::ServerPredatesKeys,
            Ok(_) => Self::Send(key),
        }
    }

    /// The key to send, when this launch sends one.
    pub(super) const fn key(&self) -> Option<&LaunchRequestKey> {
        match self {
            Self::Send(key) => Some(key),
            _ => None,
        }
    }

    /// The one stderr line an unkeyed launch prints, saying why. Never the key.
    pub(super) fn notice(&self, port: u16) -> Option<String> {
        let why = match self {
            Self::Send(_) => return None,
            Self::NoKeyInConfig => {
                "the config holds no proxy.requestKey (the server writes one when it boots)"
                    .to_string()
            }
            Self::KeyNotUrlSafe => "proxy.requestKey in the config holds a character other \
                                    than a letter, digit, '-', '.', '_' or '~', which a proxy \
                                    URL cannot carry as written"
                .to_string(),
            Self::StatusUnread(why) => {
                format!("the status of the tcr on :{port} could not be read ({why})")
            }
            Self::ServerPredatesKeys => format!(
                "the tcr on :{port} does not report requestKeyMode, so it predates request \
                 keys or holds none; a restart of it picks them up"
            ),
        };
        Some(format!("[tcr] launching without the request key: {why}"))
    }
}

/// The username a keyed proxy URL carries when it marks nothing. Any name but
/// [`tcr::request_key::CODEX_PROXY_USERNAME`] means Claude to the server.
const PLAIN_PROXY_USERNAME: &str = "tcr";

/// A loopback proxy URL for `port`, as the child's environment gets it (`env`)
/// and as a stderr line may show it (`shown`, the key as `***`).
struct ProxyUrl {
    env: String,
    shown: String,
}

/// `http://[<marker or tcr>[:<key>]@]127.0.0.1:<port>`. With no key, exactly
/// the URL each launcher set before request keys: the marker alone, or nothing.
fn loopback_proxy_url(marker: Option<&str>, key: Option<&LaunchRequestKey>, port: u16) -> ProxyUrl {
    let url = |password: Option<&str>| {
        let userinfo = match (marker, password) {
            (user, Some(password)) => {
                format!("{}:{password}@", user.unwrap_or(PLAIN_PROXY_USERNAME))
            }
            (Some(user), None) => format!("{user}@"),
            (None, None) => String::new(),
        };
        format!("http://{userinfo}127.0.0.1:{port}")
    };
    ProxyUrl {
        env: url(key.map(LaunchRequestKey::as_str)),
        shown: url(key.map(|_| "***")),
    }
}

/// SEE-THROUGH mode — the preferred route. `claude` keeps the REAL first-party
/// base URL and reaches us as a CONNECT proxy instead, so we still see (and
/// rotate) every request while Claude Code's first-party check keeps passing.
///
/// That check is a pure string compare on `ANTHROPIC_BASE_URL` — no DNS, no
/// socket, no certificate inspection, just `new URL(e).host === "api.anthropic.com"`.
/// So the fix is to stop lying to it: leave the base URL alone and move the
/// interception down a layer to `HTTPS_PROXY`, where tcr's CONNECT handler
/// MITM-terminates `api.anthropic.com` with a leaf `NODE_EXTRA_CA_CERTS` makes
/// node trust.
///
/// The proxy vars are set in BOTH cases deliberately: clients disagree about
/// which spelling they read, and one that reads only the one we skipped would go
/// direct — bypassing rotation entirely, silently.
///
/// A nested codex inherits an unmarked proxy and keeps its own login, so it
/// never needs tcr's CA. Preserve its inherited CA settings unchanged.
///
/// With [`LaunchKey::Send`] both URLs carry the request key as the password of
/// the plain `tcr` username, which Claude Code sends as `Proxy-Authorization` on
/// every `CONNECT`. Returns the stderr lines for the caller to print; they show
/// the URL with the key as `***`.
#[must_use]
pub(super) fn apply_see_through_env(
    cmd: &mut std::process::Command,
    port: u16,
    ca: &Path,
    key: &LaunchKey,
) -> Vec<String> {
    let proxy = loopback_proxy_url(key.key().map(|_| PLAIN_PROXY_USERNAME), key.key(), port);
    cmd.env("ANTHROPIC_BASE_URL", "https://api.anthropic.com");
    cmd.env("HTTPS_PROXY", &proxy.env);
    cmd.env("https_proxy", &proxy.env);
    cmd.env("NODE_EXTRA_CA_CERTS", ca);
    // Unnecessary here — we ARE first-party in this mode, so nothing gates them
    // off — but harmless, and they keep the session whole if it ever falls back.
    apply_capability_defaults(cmd);
    vec![
        format!(
            "[tcr] see-through mode: claude keeps https://api.anthropic.com, tunnelling via {}",
            proxy.shown
        ),
        see_through_ca_notice(ca),
    ]
}

/// The CA a see-through Claude session trusts; Codex's trust settings are untouched.
pub(super) fn see_through_ca_notice(ca: &Path) -> String {
    format!(
        "[tcr] trusting our MITM leaf via NODE_EXTRA_CA_CERTS={}",
        ca.display()
    )
}

/// SEE-THROUGH mode for `codex`, the only mode tcr routes it in. `codex` keeps
/// its own backend URL, the real `https://chatgpt.com` for a login served there,
/// and reaches it through us as a CONNECT proxy; while the pool holds a Codex
/// account the proxy URL's `tcr-codex` marker opts its CONNECT into interception, and
/// `CODEX_CA_CERTIFICATE` is what makes `codex` trust that leaf. A login whose
/// workspace is routed elsewhere (FedRAMP, data residency) is never pooled: tcr
/// blind-tunnels another origin, and relays a request that names its routing on
/// the client's own credential. Both proxy spellings for the reason
/// [`apply_see_through_env`] gives. Nothing of Claude's is set here: no
/// `ANTHROPIC_BASE_URL`, no `NODE_EXTRA_CA_CERTS`, no capability defaults. A
/// pooled launch adds `NODE_EXTRA_CA_CERTS` on its own, for a `claude` the
/// session starts ([`apply_nested_claude_ca`]).
///
/// With [`LaunchKey::Send`] the request key is the URL's password and the
/// `tcr-codex` username stays: a server that reports `requestKeyMode` reads the
/// marker from the username alone. Returns the stderr lines, the key as `***`.
#[must_use]
pub(super) fn apply_codex_see_through_env(
    cmd: &mut std::process::Command,
    port: u16,
    ca: &Path,
    caller_ca_exported: bool,
    key: &LaunchKey,
) -> Vec<String> {
    let proxy = loopback_proxy_url(Some(request_key::CODEX_PROXY_USERNAME), key.key(), port);
    cmd.env("HTTPS_PROXY", &proxy.env);
    cmd.env("https_proxy", &proxy.env);
    // Codex reads its extra CA from this variable (`openai/codex` http-client/src/custom_ca.rs:61);
    // NODE_EXTRA_CA_CERTS means nothing to it.
    if !caller_ca_exported {
        cmd.env("CODEX_CA_CERTIFICATE", ca);
    }
    vec![
        format!(
            "[tcr] see-through mode: codex keeps its own backend URL, proxy via {}; only marked \
             chatgpt.com traffic is eligible for pooling; custom backends, FedRAMP and \
             residency-routed logins run unpooled",
            proxy.shown
        ),
        if caller_ca_exported {
            "[tcr] keeping the caller's CODEX_CA_CERTIFICATE; its CA bundle must include tcr's \
             CA for pooled TLS"
                .to_string()
        } else {
            format!(
                "[tcr] trusting our MITM leaf via CODEX_CA_CERTIFICATE={}",
                ca.display()
            )
        },
    ]
}

/// Let a `claude` started inside a pooled codex session trust tcr's leaf: set
/// `NODE_EXTRA_CA_CERTS` to tcr's CA on the codex child, unless the caller
/// exported one (`caller_exported`), which the child then inherits untouched.
///
/// The codex child carries `HTTPS_PROXY` aimed at tcr. The nested-agent launcher
/// restores it for a `claude` started by a shell tool. It meets tcr's leaf for `api.anthropic.com`, and
/// node trusts that leaf only through this variable; without it the nested
/// session fails TLS. codex itself reads only `CODEX_CA_CERTIFICATE`, so this
/// changes nothing for codex. Silent, as the rest of the launch's defaults are.
pub(super) fn apply_nested_claude_ca(
    cmd: &mut std::process::Command,
    ca: &Path,
    caller_exported: bool,
) {
    if !caller_exported {
        cmd.env("NODE_EXTRA_CA_CERTS", ca);
    }
}

/// The host the MITM leaf must cover for `tcr run --codex` to route `codex`
/// through the pool.
pub(super) const fn mitm_codex_host() -> &'static str {
    tcr::base::provider::CODEX_HOST
}

/// BASE-URL mode — the fallback, used when see-through material is unavailable.
/// `claude` talks plain HTTP to us on loopback, which costs first-party status
/// and everything gated on it (hence [`apply_capability_defaults`]).
///
/// With [`LaunchKey::Send`] the request key goes on [`tcr::request_key::HEADER`]
/// in `ANTHROPIC_CUSTOM_HEADERS`, appended after whatever the caller (or
/// `--group`) set there ([`compose_custom_header`]). Returns the stderr lines.
#[must_use]
pub(super) fn apply_base_url_env(
    cmd: &mut std::process::Command,
    port: u16,
    key: &LaunchKey,
) -> Vec<String> {
    cmd.env("ANTHROPIC_BASE_URL", format!("http://127.0.0.1:{port}"));
    if let Some(key) = key.key() {
        let current = custom_headers_for(cmd);
        cmd.env(
            "ANTHROPIC_CUSTOM_HEADERS",
            compose_custom_header(current.as_deref(), request_key::HEADER, key.as_str()),
        );
    }
    // Here we speak plain HTTP on loopback and present NO leaf. An ambient
    // HTTPS_PROXY (e.g. another proxy on :3456) would hijack claude's traffic
    // away from us, and its NODE_EXTRA_CA_CERTS would be verifying a cert we never
    // send. Strip both so `tcr run` is self-contained and can't be captured by a
    // stale env. See-through mode does the opposite — it SETS these two, which is
    // precisely why the strip cannot live at the branch above.
    for var in [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "NODE_EXTRA_CA_CERTS",
    ] {
        cmd.env_remove(var);
    }
    apply_capability_defaults(cmd);
    vec![format!(
        "[tcr] base-URL mode: routing claude through http://127.0.0.1:{port}"
    )]
}

/// The `ANTHROPIC_CUSTOM_HEADERS` the child would get so far: the value already
/// set on `cmd` (by `--group`), none when `cmd` removes it, else this process's.
fn custom_headers_for(cmd: &std::process::Command) -> Option<String> {
    match cmd
        .get_envs()
        .find(|(name, _)| *name == "ANTHROPIC_CUSTOM_HEADERS")
    {
        Some((_, value)) => value.map(|v| v.to_string_lossy().into_owned()),
        None => std::env::var("ANTHROPIC_CUSTOM_HEADERS").ok(),
    }
}

/// Re-enable the capabilities a non-first-party `ANTHROPIC_BASE_URL` silently
/// switches off.
///
/// Claude Code gates tool search, the stall watchdog, and fine-grained tool
/// streaming on `xn()==="firstParty" && Yd()`, and base-URL mode fails that check
/// -- no error, at most a [DEBUG] line. We caused it, so we carry the
/// compensation. Never applied with the proxy down: we launch claude untouched
/// there and genuinely are first-party.
///
/// Measured 2026-07-29 against Claude Code 2.1.220 (gate at bundled-JS abs offset
/// 230310702): 1 of 4 same-day sessions lost tool search outright -- the one that
/// reached the gate ~30ms sooner, before settings.json's env block was applied to
/// process.env. Setting these here puts them in the child env at EXEC time, so
/// that ordering race cannot occur at all.
///
/// Only set what the user has not already chosen; an explicit value always wins.
fn apply_capability_defaults(cmd: &mut std::process::Command) {
    for (var, val) in [
        // Without this ~130 tool schemas load eagerly every request. Requires that we
        // forward `tool_reference` blocks upstream untouched -- we do, since
        // build_upstream_headers uses a denylist rather than an allowlist.
        ("ENABLE_TOOL_SEARCH", "true"),
        // Stall detection on the response stream; without it a hung response is never
        // proactively aborted.
        ("CLAUDE_ENABLE_BYTE_WATCHDOG", "1"),
        // Incremental tool-input streaming rather than batched delivery.
        ("CLAUDE_CODE_ENABLE_FINE_GRAINED_TOOL_STREAMING", "true"),
    ] {
        if std::env::var_os(var).is_none() {
            cmd.env(var, val);
        }
    }
}
