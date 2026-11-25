//! Port-takeover singleton: only one proxy may own the configured port.
//!
//! The recurring failure is two proxies on one port, both refreshing the same
//! accounts. Each holds its own in-memory copy of
//! the SINGLE-USE OAuth refresh tokens, so the first refresh by either revokes the
//! other's copy and accounts flap to `error` (the "token war"). On startup we
//! resolve the configured port down to exactly ONE proxy.
//!
//! **Resolving it does not have to mean killing the incumbent, and by default it
//! does not.** Both outcomes — replace, or attach-and-exit — leave one proxy
//! running, so the token war is averted either way; the difference is only WHICH
//! process survives. Killing the incumbent is by far the more expensive of the
//! two: it wipes the in-memory session→account pin map, so every live session
//! pays a full cold prompt prefix. Measured over 79.6h: 50 boots, 31 of the 49
//! gaps between them under 120s — a bare `tcr` typed to "check on things" would
//! silently take the port. So the default is now [`Takeover::IncumbentPresent`],
//! which reports and exits without binding, and the kill lives behind an explicit
//! `--replace`.
//!
//! Two safety properties, both load-bearing:
//! - **Port-scoped, not address-scoped.** A candidate is any process listening on
//!   `<configured port>` on ANY address — `127.0.0.1`, `0.0.0.0`, `[::1]`, or a LAN
//!   IP all match, exactly as the old `lsof -iTCP:<port>` did — so a proxy (or
//!   anything else) on another PORT is never touched, and a test server on an
//!   ephemeral port is safe.
//! - **Command-verified.** A candidate is signalled only if its command line is an
//!   actual `tcr` *server*. A non-proxy holder is left alone rather
//!   than killed — the bind then fails loudly with EADDRINUSE if that holder
//!   shares our bind address (in practice, loopback), but not necessarily
//!   otherwise; see [`port_listeners`]'s doc for the measured gap. This is what
//!   makes the takeover safe against a reused PID or an unrelated process on
//!   the port, whether or not the bind itself ends up refusing.
//!
//! # Identity: the owner file, then the name matcher
//!
//! Recognition used to be *only* the name matcher — `argv[0]` ending in `/tcr`.
//! That reads the HOST process, not the proxy: a proxy running inside another
//! program (the menu-bar app) reports that program's path and matches nothing, so
//! [`live_proxy_server`] returns `None`, `tcr login` stops refusing to run beside
//! a live server, and the server's next `persist_tokens` writes its boot-time
//! SINGLE-USE refresh tokens back over the fresh ones. That failure is silent
//! until every account fails to refresh at the next boot.
//!
//! So a bound proxy now ADVERTISES itself: [`write_owner_file`] drops
//! `proxy-owner-<port>.json` beside the affinity pin cache, and
//! [`classify_port_owner`] is consulted BEFORE the name matcher. Identity becomes
//! the proxy's own claim instead of an inference from someone else's `argv`.
//!
//! Three properties keep that from being a new way to get it wrong:
//! - **A stale file proves nothing.** The pid it names must ALSO appear in
//!   `port_listeners(port)` (and the port must match) or the claim is ignored
//!   entirely — a crashed proxy, or anyone at all, can leave a file behind.
//! - **A claim SUPPLEMENTS the command check, it never replaces it.** A `cli`
//!   claim is believed only when the command line ALSO says its pid is a proxy,
//!   so "command-verified" above still holds on the claim path; a pid the OS
//!   recycled onto an unrelated listener is not signalled. See [`verified_owner`].
//! - **The name matcher is RETAINED as the fallback**, so a `tcr` that predates
//!   the owner file (including one serving right now) is recognized exactly as
//!   before. Nothing has to restart for this change to be safe.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, Signal, System, UpdateKind};

/// Which proxy a recognized incumbent is. The distinction is load-bearing for
/// [`takeover_decision`], not cosmetic: a `tcr` peer is a program that is doing
/// this program's job and whose session pins cost real money to discard, while an
/// embedded one belongs to a host application that must never be signalled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyKind {
    /// The Rust proxy running as its own process — a peer of ours.
    Tcr,
    /// The Rust proxy running **inside another program** (the menu-bar app),
    /// which said so in its owner file ([`ProxyHost::Embedded`]).
    ///
    /// Never signalled, not even under `--replace`
    /// ([`incumbents_to_signal`], [`takeover_decision`]). The pid on the port is
    /// the HOST's, and `takeover_port` would SIGTERM it: AppKit installs no
    /// SIGTERM handler, so the app dies without running
    /// `applicationWillTerminate` — no graceful shutdown, no final session pin
    /// write, and the operator's windows go with it. A CLI flag must not be able
    /// to do that.
    ///
    /// This is not a new policy. `classify_proxy_server` already returns `None`
    /// for `tcr run` (below), which is exactly why a proxy hosted inside a
    /// `tcr run` process is never replaced either. Same shape, stronger reason.
    /// Stopping an embedded proxy is the host application's job.
    TcrEmbedded,
}

/// Which kind of process a proxy is running inside, as the proxy's **own claim**
/// in its owner file — never sniffed from `argv[0]`.
///
/// [`crate::proxy::server::ServeOptions::host`] carries it, and each caller states it:
/// the binary passes [`Self::Cli`], an in-process embedder passes
/// [`Self::Embedded`]. The library does not guess, because guessing is the bug
/// this whole mechanism replaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProxyHost {
    /// A standalone `tcr` process.
    Cli,
    /// A proxy served from inside a host application.
    Embedded,
    /// A host this build does not know about — a claim written by a NEWER `tcr`.
    ///
    /// The file is a cross-process, cross-BUILD contract: the `tcr` that reads it
    /// may be older than the one that wrote it. Without this catch-all an unknown
    /// `host` value fails the whole `ProxyOwner` parse, so a live proxy reads as
    /// NO CLAIM AT ALL — the name matcher then sees the host program's `argv[0]`,
    /// recognizes nothing, `tcr login` runs beside the live server and its fresh
    /// single-use refresh tokens are overwritten, and `tcr server --replace`
    /// SIGTERMs the process the claim existed to protect. Degrading to "a proxy is
    /// there, do not signal it" is strictly safer than degrading to "nothing is
    /// there", so this maps to [`ProxyKind::TcrEmbedded`].
    ///
    /// Only ever produced by `DEserialization`. Nothing in this crate writes it.
    #[serde(other)]
    Unknown,
}

impl ProxyHost {
    /// The incumbent kind a *verified* claim from this host means.
    const fn kind(self) -> ProxyKind {
        match self {
            Self::Cli => ProxyKind::Tcr,
            // An unrecognized host is treated as embedded: of the two, it is the
            // kind that is never signalled. See [`ProxyHost::Unknown`].
            Self::Embedded | Self::Unknown => ProxyKind::TcrEmbedded,
        }
    }
}

/// What a bound proxy writes to claim the port: `proxy-owner-<port>.json`.
///
/// Every field is load-bearing to a reader:
/// - `pid` is what makes a stale file harmless — it must also be listening on
///   `port` ([`verified_owner`]).
/// - `port` guards against a file being read for the wrong port (a copied file,
///   a caller passing a path that does not match its own port).
/// - `sha` is which build is serving, so an operator reading the file gets the
///   same fact the `server started` log line carries.
/// - `host` is the whole point: it says whether SIGTERM is survivable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyOwner {
    /// The pid holding the listener — the HOST process's pid when embedded.
    pub pid: u32,
    /// The port that pid bound.
    pub port: u16,
    /// The build the proxy is executing (`build_info::SHA`).
    pub sha: String,
    /// What is hosting the proxy.
    pub host: ProxyHost,
}

/// `proxy-owner-<port>.json` inside `dir` — the file name is port-scoped so two
/// proxies on two ports never overwrite each other's claim.
pub fn owner_path_in(dir: &Path, port: u16) -> PathBuf {
    dir.join(format!("proxy-owner-{port}.json"))
}

/// The directory the binary keeps the claim in: beside the session-affinity pin
/// cache, so the proxy's two pieces of cross-boot state live in one place.
pub fn default_owner_dir() -> PathBuf {
    let cache = crate::stores::affinity::default_path();
    cache
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

/// Where the binary keeps the claim for `port`. Every reader resolves the claim
/// through this function, which is why a writer must never be handed a free-form
/// path: see [`crate::proxy::server::ServeOptions::owner_dir`].
pub fn default_owner_path(port: u16) -> PathBuf {
    owner_path_in(&default_owner_dir(), port)
}

/// Write the claim atomically (0600, temp + rename — the same
/// [`crate::base::config::write_atomic`] every other state file uses, so a reader can
/// never see a half-written claim).
pub fn write_owner_file(
    path: &Path,
    owner: &ProxyOwner,
) -> Result<(), crate::base::config::ConfigError> {
    crate::base::config::write_atomic(path, &serde_json::to_string_pretty(owner)?)
}

/// Withdraw the claim at `path` — but ONLY if it still names `pid` on `port`.
///
/// The check is the whole function. `proxy-owner-<port>.json` is shared state
/// named after the PORT, not after the process, so a shutting-down proxy and its
/// successor address the same file. A shutdown frees the listener first and then
/// keeps joining background tasks (the affinity flusher does a blocking write, so
/// hundreds of milliseconds); in that window a successor can bind the port and
/// write its own claim. An unconditional unlink here would delete the SUCCESSOR's
/// live claim, and for an embedded successor — whose command line identifies
/// nothing — that means `live_proxy_server` returns `None`, `tcr login` stops
/// refusing, and the boot-time single-use refresh tokens clobber the fresh ones.
/// That is the exact failure the claim file exists to prevent, reintroduced by
/// its own cleanup path.
///
/// Best-effort past the check: a claim left behind by a crash cannot produce a
/// false positive ([`verified_owner`] re-checks the pid against the live
/// listeners and the command line), so failing to remove one costs nothing.
///
/// A read-then-unlink still has a microsecond-wide TOCTOU window, which is not
/// closable portably. It replaces a hundreds-of-milliseconds window with one
/// bounded by two syscalls; the alternative — not checking — loses the file every
/// time the race is entered at all.
pub fn remove_owner_file_if_owned(path: &Path, pid: u32, port: u16) {
    match read_owner_file(path) {
        Some(owner) if owner.pid == pid && owner.port == port => {}
        Some(owner) => {
            tracing::info!(
                path = %path.display(),
                claim_pid = owner.pid,
                claim_port = owner.port,
                our_pid = pid,
                our_port = port,
                "not withdrawing the port claim: it names another proxy, so a successor \
                 already took the port over"
            );
            return;
        }
        None => {
            // Absent (already withdrawn) or unparseable — either way it is not
            // this process's claim to delete, and a successor's write will
            // replace it.
            return;
        }
    }
    if let Err(err) = std::fs::remove_file(path)
        && err.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(
            path = %path.display(),
            error = %err,
            "could not remove the proxy owner file; a stale claim is harmless (its pid is re-checked against the live listeners)"
        );
    }
}

/// Read a claim, or `None` for absent/unreadable/unparseable. A malformed file is
/// simply not a claim — the name matcher still gets its turn.
///
/// Every failure that is not "the file is absent" is LOGGED, the read errors as
/// well as the parse errors. The claim is written 0600, so a proxy started under
/// another uid (launchd, `sudo`) leaves one a client cannot read: the read fails
/// EACCES, identity silently falls back to the matcher that by construction
/// cannot see an embedded proxy, and `tcr login` proceeds beside a live server
/// whose next persist overwrites the fresh single-use refresh tokens. An
/// unreadable claim and an absent one lead to the same fallback but they are not
/// the same fact, and the operator needs the difference on the record.
fn read_owner_file(path: &Path) -> Option<ProxyOwner> {
    let data = match std::fs::read_to_string(path) {
        Ok(data) => data,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "the proxy owner file exists but could not be READ (permissions? it is written \
                 0600, so a proxy running under another uid leaves one a client cannot open); \
                 falling back to command-line recognition, which does NOT recognize an embedded \
                 proxy"
            );
            return None;
        }
    };
    match serde_json::from_str::<ProxyOwner>(&data) {
        Ok(owner) => Some(owner),
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "proxy owner file is unreadable; falling back to command-line recognition"
            );
            None
        }
    }
}

/// The claim's VERIFICATION, as a pure function: a claim counts only when it is
/// for this `port`, its pid is one of the processes actually listening on it,
/// AND — for a claim that says it is CLI-hosted — that pid's command line is
/// still a proxy server.
///
/// This is the check that makes the owner file safe to trust. Without it the file
/// is a lie anyone can plant: any process could write `{"pid":<live proxy>,…}`
/// and change what `tcr` does about the port, and every crashed proxy would leave
/// behind a claim that outlives it and protects a pid the OS has since recycled
/// onto something unrelated.
///
/// # A claim SUPPLEMENTS the command check; it never replaces it
///
/// The pid check alone is not enough, and the difference is a killed process. A
/// crashed `tcr` leaves `{"pid":5150,"host":"cli"}` behind; months later pid 5150
/// is someone's dev server listening on the same port. `pid ∈ holders` holds, so
/// a claim-trusting reader calls it a `tcr` peer and `tcr --replace` SIGTERMs it,
/// then SIGKILLs it 800ms later. The module's "command-verified" property (see the
/// module doc) has to hold on the claim path too, so a `cli` claim is believed
/// only when [`classify_proxy_server`] ALSO recognizes the pid. The claim's job is
/// to say WHICH proxy it is and that it is ours; only the command line can say
/// that the pid is still a proxy at all.
///
/// # The one asymmetry, and why it is safe
///
/// An `embedded` claim cannot be command-verified — by construction: the pid is
/// the HOST application's and its `argv[0]` matches nothing, which is the entire
/// reason this file exists. So an embedded claim is believed on the pid check
/// alone, and that is deliberate: a false positive there can only ever make this
/// process STAND DOWN (an embedded incumbent is never signalled, on any path —
/// [`incumbents_to_signal`]), while a false positive on a CLI claim is a kill. The
/// unverifiable case is the one whose worst outcome is refusing to act.
fn verified_owner(
    owner: Option<&ProxyOwner>,
    port: u16,
    holders: &[u32],
    command: impl Fn(u32) -> Vec<String>,
) -> Option<Incumbent> {
    let owner = owner?;
    if owner.port != port || !holders.contains(&owner.pid) {
        return None;
    }
    let kind = owner.host.kind();
    if owner.host == ProxyHost::Cli && classify_proxy_server(&command(owner.pid)).is_none() {
        tracing::warn!(
            pid = owner.pid,
            port,
            "ignoring a cli-hosted port claim whose pid is not running a proxy: the claim is \
             stale and the OS has recycled that pid onto something else"
        );
        return None;
    }
    Some(Incumbent {
        pid: owner.pid,
        kind,
    })
}

/// The owner file at `owner_path`, verified against the processes actually
/// holding `port` and against the claimed pid's command line. `None` when there is
/// no claim, the claim is for another port, its pid is not listening, or a
/// CLI-hosted claim names a pid that is not a proxy — in every one of those cases
/// the caller falls back to the name matcher.
///
/// `holders`, `owner_path` and `command` are parameters rather than read in here
/// so this is testable against a real file without a real proxy.
pub fn classify_port_owner(
    port: u16,
    holders: &[u32],
    owner_path: &Path,
    command: impl Fn(u32) -> Vec<String>,
) -> Option<Incumbent> {
    verified_owner(read_owner_file(owner_path).as_ref(), port, holders, command)
}

/// A recognized, replaceable proxy holding the port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Incumbent {
    pub pid: u32,
    pub kind: ProxyKind,
}

/// Does this argv look like a `tcr` *server* — a process safe to replace as a
/// proxy incumbent on the port? Recognizes `…/tcr` at argv0, with the `server`
/// subcommand, no subcommand, or a flag — since bare `tcr` defaults to the
/// server. Never matches a bare `grep`/editor/`tcr run`/etc.
///
/// Takes pre-split argv (`&[String]`, as [`sysinfo::Process::cmd`] returns it —
/// kernel-tokenized from `/proc/<pid>/cmdline` on Linux, `libproc` argv on macOS),
/// never a joined command LINE. A joined line has to be re-split by whitespace to
/// be examined, and that re-split mis-tokenizes any executable path containing a
/// space: `"/Applications/My App/tcr server"` used to split into
/// `["/Applications/My", "App/tcr", "server"]`, so `tokens[0]` never ended with
/// `/tcr` and a REAL tcr server went unrecognized. Taking the kernel's own argv
/// removes that defect class instead of patching the symptom — there is
/// deliberately no `&str`/whitespace-splitting overload here, because a
/// convenience wrapper that re-introduces the split is the same bug behind a
/// fixed test case.
pub fn is_proxy_server(argv: &[String]) -> bool {
    classify_proxy_server(argv).is_some()
}

/// [`is_proxy_server`] plus WHICH proxy it is. Same recognition rules — the bool
/// version delegates here, so the two can never drift apart.
pub fn classify_proxy_server(argv: &[String]) -> Option<ProxyKind> {
    if argv.is_empty() {
        return None;
    }
    let is_tcr = |t: &str| t == "tcr" || t.ends_with("/tcr");

    // `tcr` at argv0. A server unless the subcommand is a recognized
    // NON-server verb; bare `tcr`, `tcr server`, and `tcr --flag` are all servers.
    if is_tcr(&argv[0]) {
        return match argv.get(1).map(String::as_str) {
            None => Some(ProxyKind::Tcr),
            Some(t) if t.starts_with('-') || t == "server" => Some(ProxyKind::Tcr),
            Some(_) => None, // `tcr run`, `tcr status`, `tcr accounts`, …
        };
    }

    None
}

/// The argv of `pid` (via [`sysinfo`], no subprocess); empty if the process is
/// gone or its cmdline could not be read.
///
/// A fresh [`System`] is built per call rather than kept and re-refreshed,
/// deliberately: `pid` is reused by the OS, and a stale snapshot could hand back
/// a RECYCLED process's cmdline. `ProcessRefreshKind::nothing().with_cmd(Always)`
/// is required — the crate's own `refresh_processes` convenience method does not
/// populate `cmd` at all (its default `UpdateKind` for `cmd` is `Never`), so a
/// caller reaching for that shortcut here would get an empty argv on every call
/// and every proxy would silently stop being recognized.
fn process_command(pid: u32) -> Vec<String> {
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
        false,
        ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always),
    );
    sys.process(Pid::from_u32(pid)).map_or_default(|p| {
        p.cmd()
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect()
    })
}

/// PIDs listening on `:port` on any address (via [`listeners`], no subprocess),
/// each pid appearing at most once. Empty on any failure — an erroring
/// [`listeners::get_all`] degrades to no signal, and to binding anyway; it is
/// not "no takeover" either: [`takeover_decision`]`(&[], _)` is
/// `Takeover::Proceed`. That is safe for two of the three callers and not for
/// the third, and only PARTIALLY safe even there. [`takeover_port`] and
/// `server.rs`'s boot path both then actually BIND, and `EADDRINUSE` is a
/// property of the `(address, port)` PAIR, not the port — and the platforms
/// this crate ships to disagree on what that licenses (measured via
/// `std::net::TcpListener::bind`, the same constructor as `server.rs:784`;
/// see `scripts/FINDINGS-bind-overlap.md`): on Linux the backstop fires for
/// every pairing measured, but on macOS it fires only against a SAME-address
/// (loopback) incumbent — in practice another `tcr`, which hard-codes
/// `127.0.0.1` — and coexists silently with a WILDCARD-bound one (`0.0.0.0`
/// or `::`), which this module's own doc (`:22`) already lists as a
/// candidate. Both platforms set `SO_REUSEADDR` (Rust std does so
/// unconditionally on Unix); they disagree on whether it licenses overlap
/// between a live wildcard and a live specific-address listener. On macOS,
/// against a wildcard incumbent, an enumeration failure means we could end up
/// bound ALONGSIDE it, silently, risking the two proxies token-war — the
/// platform where this matters most, since `TcrBar.app` and the live proxy
/// run there. This gap is inherited, not introduced by the dedup or logging
/// above: neither the old `lsof` scan nor the new one filters by address, so
/// a working scan already sees and handles a wildcard holder — only the
/// failure-collapse path lets one slip past unbound-against. Empty on
/// enumeration failure is unchanged by any of this — "never a false kill" is
/// still true and load-bearing throughout — but "no takeover" is not a
/// consequence of it, and where a bind fails to actually refuse depends on
/// the incumbent's address AND the platform, not merely on the port being
/// held. `login::login`'s
/// guard binds nothing at all, so no backstop of any strength applies to it:
/// on an enumeration failure it proceeds beside a live server it failed to
/// detect, and the server's next `persist_tokens` writes its boot-time tokens
/// back over the ones the login just wrote, clobbering the single-use refresh
/// tokens (observed live 2026-07-19, see `oauth.rs:788-793`).
///
/// Empty-on-failure is parity with the old `lsof`-based implementation only on
/// the `Err` path — `lsof` exiting 1 on a real failure and on an ordinary free
/// port were indistinguishable, so the old code degraded to empty either way,
/// exactly as the `Err` arm below does, and it is now LOGGED rather than
/// silently discarded (see the `Err` arm). But `get_all()` has a second failure
/// shape with no `lsof` analogue at all, because it never reaches this
/// function's `Err` arm: on macOS, a listener whose process name cannot be read
/// or is not valid UTF-8 is silently DROPPED from an otherwise-`Ok` result
/// (`listeners-0.6.1/src/platform/macos/mod.rs:35`, gated on
/// `proc_names_cache.get(pid)` returning `None`; the read itself can fail two
/// ways in `proc_name.rs:27-29` and `:35-37`). `lsof -t` never looks at a
/// process name, so it has no equivalent gap. This is upstream's to fix, not
/// ours to work around here — documented so a future "why did this holder go
/// unseen" investigation does not restart from zero. The axis where the new
/// code is strictly better: the old path failed OPEN wherever `lsof` itself was
/// absent, which is routine on Linux and in containers — that external-binary
/// dependency is gone entirely.
///
/// Deliberately `listeners::get_all()` filtered to [`listeners::SocketState::Listen`]
/// ourselves, NEVER [`listeners::get_process_by_port`]. That shortcut applies no
/// socket-state filter at all (upstream `listeners` issue #36, open,
/// maintainer-confirmed) and returns the FIRST socket matching the port —
/// `sshd` `LISTENing` on 22 and `sshd-session` ESTABLISHED on the same local port
/// both key to port 22, and the shortcut can hand back the child. Our old
/// `lsof -sTCP:LISTEN` excluded non-listening sockets specifically so that a
/// pid merely ESTABLISHED as a *client* of our port could never reach
/// `replaceable_incumbents` and, under `--replace`, the SIGKILL loop. The
/// explicit state filter below is what reproduces that guarantee. It also sides
/// around `get_process_by_port`'s separate `port == 0` error, which we would
/// otherwise have to special-case for the ephemeral-port test harness.
///
/// The protocol filter is the same kind of parity restoration, made explicit
/// rather than left to rest on an upstream implementation detail: the old
/// `lsof -iTCP:{port}` excluded UDP outright. Today, both `listeners` backends
/// hard-code every UDP entry's state to `SocketState::Unknown` (never `Listen`)
/// (`platform/linux/proto_listener.rs:128,165`, `platform/macos/c_socket_fd_info.rs:32`),
/// so the state filter above already excludes UDP as a side effect — but
/// `SocketState` is a public enum upstream is still evolving (see issue #36),
/// and a future release giving UDP a real state would silently start admitting
/// UDP holders here with no line of code to blame. Filtering on
/// `Protocol::TCP` directly keeps that guarantee independent of an
/// undocumented invariant in a dependency.
///
/// **Dedup, order-preserving.** One process can hold several LISTEN sockets on
/// the same port at once — the common case is a dual-stack bind,
/// `127.0.0.1:<port>` plus `[::1]:<port>` from one `bind(0.0.0.0)`/`bind(::)`
/// call, which is how a Node HTTP server listens by default. [`listeners::Listener`]
/// derives `Hash`/`Eq` over `{process, socket, protocol, state}`, and `socket` is
/// part of that key, so `get_all()`'s internal `HashSet` keeps both sockets as
/// separate entries — the same pid then appears twice in this function's output.
/// The old `lsof -t` collapsed that to one line per process; below reproduces
/// that collapse explicitly, in FIRST-SEEN order, because callers
/// ([`live_proxy_server`] via `.next()`, [`takeover_decision`] via `.find()`)
/// read the first element as *the* incumbent to report, and reordering holders
/// would change which one that is.
///
/// Opposite divergence on Linux, needing no fix here: that backend's
/// inode→process map uses last-writer-wins `HashMap::insert`
/// (`platform/linux/helpers.rs:53-54`), so a listening fd shared across a fork
/// family collapses to one pid rather than macOS's several — untested by CI
/// despite running on `ubuntu-latest`, since nothing here forks a listener.
fn port_listeners(port: u16) -> Vec<u32> {
    let mut seen = HashSet::new();
    let all = match listeners::get_all() {
        Ok(all) => all,
        Err(error) => {
            // No silent fallbacks: an enumeration failure still degrades to "no
            // holders" (empty-on-failure is load-bearing, see the doc above),
            // but that degradation must be visible, not invisible — this is the
            // one signal `lsof`'s `Ok`/exit-1 conflation could never produce.
            tracing::warn!(port, %error, "listeners::get_all failed; treating the port as unheld");
            Default::default()
        }
    };
    all.into_iter()
        .filter(|listener| listener.protocol == listeners::Protocol::TCP)
        .filter(|listener| listener.state == listeners::SocketState::Listen)
        .filter(|listener| listener.socket.port() == port)
        .map(|listener| listener.process.pid)
        .filter(|&pid| seen.insert(pid))
        .collect()
}

/// The pure takeover DECISION: of the processes holding the port, which are
/// replaceable proxy incumbents, and which proxy is each? Excludes self and any
/// non-proxy holder. Split out from the side-effecting kill so it is unit-testable
/// with injected inputs.
///
/// Name matching only — [`replaceable_incumbents_with_owner`] is the version that
/// takes a verified owner-file claim into account.
pub fn replaceable_incumbents(
    holders: &[u32],
    self_pid: u32,
    command: impl Fn(u32) -> Vec<String>,
) -> Vec<Incumbent> {
    replaceable_incumbents_with_owner(holders, self_pid, command, None)
}

/// [`replaceable_incumbents`] with the port's **verified** owner-file claim
/// (`classify_port_owner`) consulted FIRST.
///
/// Order matters: the claim is the proxy's own statement about what is hosting it,
/// while the command line is an inference from the host process's `argv[0]` — and
/// for an embedded proxy that inference is wrong in the dangerous direction (it
/// recognizes nothing at all). Where both speak, the claim wins.
///
/// `owner` must already be verified against the live listeners AND (for a `cli`
/// claim) against the pid's command line; pass the result of
/// [`classify_port_owner`], never a raw file read. That is what keeps this
/// short-circuit from being a way around the command check — by the time a claim
/// arrives here, the command check has already had its say.
pub fn replaceable_incumbents_with_owner(
    holders: &[u32],
    self_pid: u32,
    command: impl Fn(u32) -> Vec<String>,
    owner: Option<Incumbent>,
) -> Vec<Incumbent> {
    holders
        .iter()
        .copied()
        .filter(|&pid| pid != self_pid)
        .filter_map(|pid| match owner.filter(|owner| owner.pid == pid) {
            Some(claimed) => Some(claimed),
            None => classify_proxy_server(&command(pid)).map(|kind| Incumbent { pid, kind }),
        })
        .collect()
}

/// Detection-only: the PID of a live `tcr` *server* currently holding
/// `port`, if any. Reuses the exact port-scoped, command-verified decision as
/// [`takeover_port`] ([`replaceable_incumbents`]) but signals NOTHING — `tcr login` uses
/// it to REFUSE to run beside a live server (the server reads config only at boot,
/// and its next `persist_tokens` writes its boot-time TOKENS back over the file,
/// clobbering the login's fresh ones). Returns the first replaceable
/// [`Incumbent`]; `None` when the port is free or held only by a non-proxy
/// process.
///
/// The KIND travels with the pid, and it has to. Since the owner file, this can
/// return the pid of a HOST APPLICATION serving the proxy in-process, and a
/// caller that renders "kill {pid}" would be telling the operator to SIGTERM a
/// GUI that installs no handler for it — no `applicationWillTerminate`, no final
/// session→account pin write. A bare `u32` cannot carry that distinction, so it
/// is not returned as one; see `login::login_guard_refusal`.
pub fn live_proxy_server(port: u16) -> Option<Incumbent> {
    let holders = port_listeners(port);
    let owner = classify_port_owner(port, &holders, &default_owner_path(port), process_command);
    replaceable_incumbents_with_owner(&holders, std::process::id(), process_command, owner)
        .into_iter()
        .next()
}

/// Is `pid` still alive? Uses the same `refresh_processes_specifics` /
/// `ProcessesToUpdate::Some` shape as [`signal_pid`] (both pass
/// `ProcessRefreshKind::nothing()`, no `cmd`), minus the signal — a fresh
/// snapshot per call so a reused pid cannot read as the process we just
/// signalled. [`process_command`] is the odd one out: it refreshes the same way
/// but additionally requests `.with_cmd(UpdateKind::Always)`, since it is the
/// only one of the three that needs the command line.
fn is_alive(pid: u32) -> bool {
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
        true,
        ProcessRefreshKind::nothing(),
    );
    sys.process(Pid::from_u32(pid)).is_some()
}

/// How long [`takeover_port`] gives a `SIGTERMed` incumbent to exit on its own
/// before escalating to SIGKILL.
///
/// A **ceiling, not a floor**: [`wait_for_exit`] returns the moment the process
/// is gone. This used to be a flat `sleep`, and that is why a bounce cost about
/// a second of refused connections even when the incumbent had already exited.
/// Measured 2026-09-13 across three restarts, the port sat unbound for 1.124s,
/// 0.976s and 1.020s (`no longer accepting` to `server started` in
/// the server's own log), nearly all of it spent sleeping out this
/// window after the incumbent was already gone.
const TERM_GRACE: Duration = Duration::from_millis(800);

/// How long to wait after SIGKILL for the process table entry to clear. Same
/// ceiling-not-floor rule as [`TERM_GRACE`].
const KILL_GRACE: Duration = Duration::from_millis(300);

/// How often [`wait_for_exit`] re-checks. Each check is a fresh one-pid
/// `sysinfo` refresh, so this bounds both the overshoot past the real exit (one
/// interval) and the syscall load during the wait (at most 80 checks across
/// [`TERM_GRACE`]).
const EXIT_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Wait up to `grace` for `pid` to leave the process table.
///
/// Returns `true` as soon as it has, `false` if it was still there when `grace`
/// ran out. A caller that gets `false` holds exactly what the old flat `sleep`
/// plus [`is_alive`] pair told it, and holds it no later.
///
/// **Never slower than the ceiling it replaces.** A zombie still occupies the
/// process table, so an incumbent whose parent has not reaped it yet keeps this
/// polling for the whole of `grace` — identical to the `sleep` this replaces,
/// not a regression on it.
fn wait_for_exit(pid: u32, grace: Duration) -> bool {
    let deadline = std::time::Instant::now() + grace;
    loop {
        if !is_alive(pid) {
            return true;
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return false;
        }
        sleep(EXIT_POLL_INTERVAL.min(remaining));
    }
}

/// Send `signal` to `pid` via [`sysinfo::Process::kill_with`] — never
/// [`sysinfo::Process::kill`], which always sends `SIGKILL` regardless of what
/// the caller asked for and would silently delete the graceful-shutdown window
/// between the `Signal::Term` and `Signal::Kill` calls in [`takeover_port`].
///
/// `kill_with` returns `None` when `signal` is unsupported on the current
/// platform — that case is logged, not swallowed, because a caller reading
/// "we tried to signal it" from a `None` it never checked is exactly how a
/// process that was never actually asked to stop ends up presumed dead.
fn signal_pid(pid: u32, signal: Signal) {
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
        true,
        ProcessRefreshKind::nothing(),
    );
    let Some(process) = sys.process(Pid::from_u32(pid)) else {
        // Already gone; nothing to signal.
        return;
    };
    match process.kill_with(signal) {
        Some(true) => {}
        Some(false) => {
            tracing::warn!(pid, ?signal, "failed to signal process");
        }
        None => {
            tracing::warn!(
                pid,
                ?signal,
                "this signal is not supported on the current platform; the process was NOT signalled"
            );
        }
    }
}

/// What the caller must do after [`takeover_port`] has looked at the port.
///
/// The enum exists so the decision stays HERE — with the port-scoped,
/// command-verified knowledge of who holds it — while the exit belongs to
/// `main`, which owns the process's exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Takeover {
    /// Nothing replaceable is in the way (the port is free, held only by a
    /// non-proxy, or the incumbent was just replaced). Bind.
    Proceed,
    /// A recognized proxy incumbent holds the port and was deliberately left
    /// running. The caller must NOT bind; it should exit 0.
    ///
    /// Carries the whole [`Incumbent`], not just its pid. The KIND decides what a
    /// caller may tell the operator to do about it — a `tcr` peer can be signalled
    /// and an embedded one must only be quit — and a caller handed a bare `u32`
    /// has to re-scan the incumbent list to recover a fact this function already
    /// looked up, which is exactly the kind of parallel predicate that drifts.
    IncumbentPresent(Incumbent),
}

/// The pure decision: given the replaceable incumbents on the port and whether
/// `--replace` was passed, do we bind or stand down?
///
/// Split out from the side effects for the same reason as
/// [`replaceable_incumbents`] — and it is the single gate on the kill loop below,
/// so a test that pins this function pins whether a live proxy gets signalled.
///
/// A [`ProxyKind::Tcr`] incumbent is the one that is protected by default: it is
/// doing this program's job, and replacing it wipes its session→account pin map.
///
/// A [`ProxyKind::TcrEmbedded`] incumbent is checked BEFORE `replace`, and is the
/// one kind `--replace` cannot override: the pid on the port belongs to the host
/// application, so replacing it means `SIGTERMing` a GUI that installs no handler
/// for it. See [`ProxyKind::TcrEmbedded`].
///
/// That stand-down decides only whether WE bind. It says nothing about the OTHER
/// incumbents on the port — see [`incumbents_to_signal`], which partitions them by
/// kind rather than reading this one verdict as "touch nothing".
pub fn takeover_decision(replaceable: &[Incumbent], replace: bool) -> Takeover {
    if let Some(embedded) = replaceable
        .iter()
        .find(|incumbent| incumbent.kind == ProxyKind::TcrEmbedded)
    {
        return Takeover::IncumbentPresent(*embedded);
    }
    if replace {
        return Takeover::Proceed;
    }
    match replaceable
        .iter()
        .find(|incumbent| incumbent.kind == ProxyKind::Tcr)
    {
        Some(peer) => Takeover::IncumbentPresent(*peer),
        None => Takeover::Proceed,
    }
}

/// Which of the recognized incumbents this run will actually signal.
///
/// Gated on [`takeover_decision`] rather than reimplementing its reasoning, so
/// "we proceeded" and "we signalled exactly these" cannot drift apart: a
/// stand-down signals NOTHING. Half-killing there would leave the port held by
/// the survivor and this process refusing to bind anyway — all cost, no benefit.
///
/// Past that gate: with `--replace`, every incumbent. Without it, none: a run
/// that reaches `Proceed` unasked found no incumbent to signal.
///
/// A [`ProxyKind::TcrEmbedded`] incumbent is filtered out unconditionally, on top
/// of the stand-down gate that already covers it. Both, deliberately: signalling
/// one kills a host application without its `applicationWillTerminate`, so the
/// guarantee is worth stating twice rather than resting on a `!= Proceed` that a
/// future edit to [`takeover_decision`] could relax by accident.
///
/// # The one stand-down that is not "touch nothing": `--replace` past an embedded
///
/// `port_listeners` filters by port, not by address, so a `tcr server` on
/// `[::1]:3456` and an embedded tcr on `127.0.0.1:3456` are both holders.
/// Reading the embedded stand-down as "signal nothing" would make that second
/// proxy undisplaceable on EVERY path — `--replace` included, since the embedded
/// check precedes it — and the two proxies would then token-war over the same
/// single-use refresh tokens forever, which is the failure this module exists to
/// end. So the kinds are partitioned rather than collapsed: the embedded
/// incumbent is never signalled and still stands us down, while an explicit
/// `--replace` still reaches the NON-embedded incumbents beside it. That is the
/// only way to honour both rules at once.
///
/// Without `--replace` nothing is signalled at all, embedded or not: the
/// operator did not ask for a kill.
fn incumbents_to_signal(replaceable: &[Incumbent], replace: bool) -> Vec<Incumbent> {
    if !replace {
        return Vec::new();
    }
    let embedded_present = replaceable
        .iter()
        .any(|incumbent| incumbent.kind == ProxyKind::TcrEmbedded);
    let stood_down = takeover_decision(replaceable, replace) != Takeover::Proceed;
    if stood_down && !embedded_present {
        return Vec::new();
    }
    replaceable
        .iter()
        .copied()
        .filter(|incumbent| incumbent.kind != ProxyKind::TcrEmbedded)
        .collect()
}

/// A marker phrase in [`stand_down_message`] that a SECOND codebase parses.
///
/// TcrBar (`apps/macos`) supervises a spawned `tcr server` by scanning its
/// stderr: `ServerController.incumbentMarkers` turns a match into
/// `.incumbentHoldsPort` (a benign report) or `.takeoverRefused` (the takeover
/// did not happen), and a miss into a bare `.exited(0)` — which for a stand-down
/// would render "the server exited cleanly" for a server that never bound.
/// The phrase is load-bearing across a language boundary that no compiler
/// checks, so it is a named constant with a test, not an inline literal.
pub const INCUMBENT_MARKER: &str = "another proxy holds";

/// What we print before standing down, as a pure function so the cross-language
/// marker contract above is testable.
///
/// Says "is listening", NOT "is healthy". All we established is that a
/// command-verified proxy holds the port (`replaceable_incumbents` ->
/// `classify_proxy_server`);
/// nothing here probes whether it still serves. A wedged proxy holds its port just
/// fine, and the build line printed right after this one can say the incumbent never
/// answered — so claiming health here would both overstate the check and contradict
/// the next line. Assert only what the code checked.
fn stand_down_message(port: u16, pid: u32) -> String {
    format!(
        "[tcr] {INCUMBENT_MARKER} :{port} (pid {pid}) and it is still listening — leaving it \
         alone and exiting without binding. Replacing it would wipe its session→account pin map \
         and cold-start every live session's prompt cache, the most expensive event in this \
         system. Stop it, or use --replace if it runs tcr {} or newer.",
        crate::base::build_info::HANDOFF_SINCE
    )
}

/// The stand-down message for an EMBEDDED incumbent — the one case `--replace`
/// does not override, so the message must not offer it.
///
/// Carries the same [`INCUMBENT_MARKER`] the CLI message does, because TcrBar's
/// `incumbentMarkers` scan is what turns this into a reported incumbent instead of
/// a bare `exited(0)`; only the instruction differs, and it names the host
/// application, which is the one place that can actually stop this proxy.
fn embedded_stand_down_message(port: u16, pid: u32) -> String {
    format!(
        "[tcr] {INCUMBENT_MARKER} :{port} (pid {pid}) and it is served from inside its host \
         application — leaving it alone and exiting without binding. --replace cannot take this \
         one over: the pid is the app's, and signalling it would kill the app without its normal \
         shutdown, losing the session→account pin map. Quit the host application to stop this \
         proxy."
    )
}

/// The stand-down line for `incumbent` on `port`, carrying [`INCUMBENT_MARKER`].
/// An embedded incumbent gets the line that names its host application instead
/// of offering `--replace`. [`takeover_port`] prints it; a caller that stands
/// down without going through `takeover_port` (the CLI's `--replace`, which
/// serves with the never-signal policy) prints it itself.
pub fn stand_down_notice(port: u16, incumbent: Incumbent) -> String {
    if incumbent.kind == ProxyKind::TcrEmbedded {
        embedded_stand_down_message(port, incumbent.pid)
    } else {
        stand_down_message(port, incumbent.pid)
    }
}

/// Resolve `port` down to one proxy for this server.
///
/// With `replace`, a recognized proxy incumbent is terminated (SIGTERM, then
/// SIGKILL a survivor after a grace) so this instance can bind. Without it — the
/// DEFAULT — a `tcr` incumbent is reported and left running, and the caller is
/// told to exit rather than bind. A non-proxy holder is reported and
/// LEFT ALONE in every case — the bind then fails loudly with EADDRINUSE if
/// that holder shares our bind address (in practice, loopback); against a
/// wildcard-bound holder the bind is not guaranteed to refuse, see
/// [`port_listeners`]'s doc for the measured gap.
#[must_use = "the caller must exit instead of binding on IncumbentPresent"]
pub fn takeover_port(port: u16, replace: bool) -> Takeover {
    let holders = port_listeners(port);
    let owner = classify_port_owner(port, &holders, &default_owner_path(port), process_command);
    let replaceable =
        replaceable_incumbents_with_owner(&holders, std::process::id(), process_command, owner);

    // Surface a non-proxy holder (we won't kill it; the bind will fail).
    for &pid in &holders {
        if pid != std::process::id() && !replaceable.iter().any(|i| i.pid == pid) {
            let cmd = process_command(pid);
            if !cmd.is_empty() {
                eprintln!(
                    "[tcr] :{port} is held by a non-proxy process (pid {pid}): {} — not replacing it; the bind will fail if it stays.",
                    cmd.join(" ")
                );
            }
        }
    }

    let decision = takeover_decision(&replaceable, replace);
    if let Takeover::IncumbentPresent(incumbent) = decision {
        // Only the pid and the instruction live here; `main` prints the build
        // comparison, because THAT is the part a user typing `tcr` after a rebuild
        // actually needs and it requires an async status read we must not do here.
        //
        // The kind comes from the decision itself rather than a second scan of
        // `replaceable` for the same predicate: two copies of "is this one
        // embedded?" is how the generic message — which offers `--replace` — ends
        // up printed for the one kind `--replace` cannot take over.
        eprintln!("{}", stand_down_notice(port, incumbent));
    }

    // Not `else`: standing down for an EMBEDDED incumbent still lets an explicit
    // `--replace` displace the non-embedded proxies sharing the port. See
    // `incumbents_to_signal`; without it a second proxy co-resident with an
    // embedded tcr is undisplaceable on every path.
    for Incumbent { pid, kind } in incumbents_to_signal(&replaceable, replace) {
        match kind {
            // Unreachable by construction — `incumbents_to_signal` filters this
            // kind out twice. It is handled rather than `unreachable!()`d because
            // the cost of being wrong is a SIGTERM to the operator's menu-bar app:
            // skip and say so, never panic and never signal.
            ProxyKind::TcrEmbedded => {
                tracing::warn!(
                    port,
                    pid,
                    "refusing to signal an embedded proxy: the pid belongs to its host application"
                );
                continue;
            }
            ProxyKind::Tcr => eprintln!(
                "[tcr] replacing existing proxy on :{port} (pid {pid}) — one proxy per port, or the two mutually invalidate each other's single-use refresh tokens (token war)."
            ),
        }
        // The other half of the boot marker: in TUI mode this eprintln lands on a
        // terminal that the alternate screen is about to cover, so the durable log
        // is the only place a takeover is recoverable. Pairs with "server started"
        // to turn "the server bounced" into "pid N was killed by pid M".
        tracing::info!(port, replaced_pid = pid, "replacing incumbent proxy");
        // Graceful first: SIGTERM, then a grace window, then SIGKILL only if it
        // survived. `signal_pid` calls `kill_with`, never `kill` (which always
        // sends SIGKILL) — collapsing these two calls onto `kill` would delete
        // this exact grace window and cost every live session its prompt cache.
        //
        // The grace is a CEILING. `wait_for_exit` returns the instant the
        // incumbent is gone, so the successor binds as soon as the port can
        // actually be had rather than sleeping out a fixed window the incumbent
        // already finished inside. See `TERM_GRACE` for the measurement.
        signal_pid(pid, Signal::Term);
        if !wait_for_exit(pid, TERM_GRACE) {
            signal_pid(pid, Signal::Kill);
            // The verdict is deliberately not checked: after SIGKILL we proceed
            // either way, exactly as the old unconditional 300ms sleep did. The
            // bind is what actually adjudicates whether the port came free.
            wait_for_exit(pid, KILL_GRACE);
        }
    }
    decision
}

#[cfg(test)]
mod tests;
