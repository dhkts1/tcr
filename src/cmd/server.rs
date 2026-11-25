//! `tcr server`, the proxy itself: load the config, serve until a shutdown trigger, and turn a
//! stand-down into the process exit code.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use tcr::base::config::{self, Config, ConfigError};
use tcr::proxy::live_client;
use tcr::{
    base::build_info, base::config_keys, proxy::server, stores::affinity, stores::singleton,
    verbs::login, verbs::tui,
};

use super::ConfigFile;

mod logging;
#[cfg(test)]
mod tests;

use logging::init_tracing;

#[derive(clap::Args)]
pub(crate) struct ServerArgs {
    /// Port to bind (overrides `proxy.port` from the config).
    #[arg(long)]
    port: Option<u16>,
    #[command(flatten)]
    config: ConfigFile,
    /// Run without the TUI, logging to stdout.
    #[arg(long)]
    headless: bool,
    /// Take over the listening socket without unbinding the port. The old server
    /// drains its accepted connections. An older server without handoff support
    /// is left running and must be stopped explicitly once to upgrade.
    #[arg(long)]
    replace: bool,
    /// DEPRECATED and now a no-op: this is the default. Kept accepted so existing
    /// scripts and launch agents that pass it keep working. Pass `--replace` for
    /// the old default (take the port over).
    ///
    /// `conflicts_with` rather than a silent precedence rule: the two flags are a
    /// contradiction, and the previous wiring resolved it by quietly discarding
    /// `--replace`. An operator whose launchd plist or shell alias already carries
    /// `--no-replace`, adding `--replace` to force a rebuilt binary onto the port,
    /// got a stand-down and exit 0 — while `--help` told them the flag they left
    /// in place does nothing. clap now rejects the pair by name, which is the only
    /// outcome that cannot be misread.
    #[arg(long, conflicts_with = "replace")]
    no_replace: bool,
}

/// A stand-down that resolved cleanly: a peer proxy holds the port and is
/// serving this binary's code (or something we have no reason to doubt).
const EXIT_STOOD_DOWN_OK: i32 = 0;
/// A stand-down where the incumbent is serving a DIFFERENT commit than the
/// binary that was just run.
///
/// `cargo build && tcr` used to GUARANTEE the new binary was serving; standing
/// down silently broke that guarantee, and a warning on stderr is routinely
/// unread in a headless or piped context. This is the machine-readable half, so
/// `tcr && <next step>` stops instead of proceeding as if the new build were
/// live. Not `1`: that is a genuine startup failure, and not `2`, which clap
/// uses for a usage error.
const EXIT_STOOD_DOWN_STALE: i32 = 3;
/// A stand-down where the incumbent never answered the liveness probe — it holds
/// the listening socket and serves nothing. Distinct from [`EXIT_STOOD_DOWN_STALE`]
/// because the operator's next command is different: `--replace` is a recovery
/// here, not an upgrade.
const EXIT_STOOD_DOWN_NOT_ANSWERING: i32 = 4;

/// The stand-down's exit code, as a pure function of what was actually measured.
///
/// Keyed on the probe's verdict values, never on the rendered sentence: an exit
/// code grepped out of our own prose is a gate any rewording silently disarms.
///
/// Liveness outranks build skew because it is the more urgent fact — nothing is
/// serving at all — and because a proxy that would not answer also could not
/// report a build, so its build verdict is `Unknown` by construction.
const fn stand_down_exit_code(
    liveness: &live_client::Liveness,
    verdict: build_info::StandDownBuild,
) -> i32 {
    if matches!(liveness, live_client::Liveness::Silent { .. }) {
        return EXIT_STOOD_DOWN_NOT_ANSWERING;
    }
    match verdict {
        build_info::StandDownBuild::Stale => EXIT_STOOD_DOWN_STALE,
        // `Unknown` stays 0: an older tcr that answers but ships no build stamp
        // is a working proxy, and failing every such start would be noise for a
        // question that was never answered either way.
        build_info::StandDownBuild::InSync
        | build_info::StandDownBuild::DirtyBuild
        | build_info::StandDownBuild::Unknown => EXIT_STOOD_DOWN_OK,
    }
}

/// `tcr server` — the clap→[`server::ServeOptions`] adapter.
///
/// Everything that actually boots the proxy lives in [`tcr::proxy::server`],
/// which is usable from a test or any other embedder. What stays here is what
/// only a *binary* may do: choose the logging subscriber, print the operator's
/// stand-down diagnosis, turn that stand-down into a process exit code, and pick
/// how to wait (the TUI, or a headless block on Ctrl-C).
pub(crate) async fn run_server(args: ServerArgs) -> anyhow::Result<()> {
    let config_path = args.config.path();
    let (config, persist_path) = load_config(&config_path)?;
    let restart_endpoint = match tcr::stores::restart::Endpoint::bind() {
        Ok(endpoint) => Some(endpoint),
        Err(error) => {
            eprintln!("[tcr] warning: listener handoff disabled: {error}");
            None
        }
    };
    let handover = if args.replace {
        tcr::stores::restart::request_listener(args.port.unwrap_or(config.proxy.port)).await?
    } else {
        None
    };
    // The predecessor's final token flush precedes sending the socket. Read it
    // now, not from the snapshot taken before asking the predecessor to stop.
    let (config, persist_path) = if handover.is_some() {
        load_config(&config_path)?
    } else {
        (config, persist_path)
    };

    // A first boot with an empty fleet imports this machine's Claude Code
    // login, if it has one — here, BEFORE the manager and the fleet are built
    // from `config`, so the very first request already has an account to serve
    // it rather than a 429 until someone runs `tcr login`. Never on a config
    // that already has accounts (see `auto_import_claude_code_login`), and
    // never fatal: the failure paths all warn and boot with zero accounts,
    // exactly as this line did before.
    let config = if config.holds_no_accounts() && !config.refuses_claude_code_import() {
        match login::auto_import_claude_code_login(&config_path).await {
            // Re-read: the import wrote the file, and re-reading is what makes
            // the running fleet the one on disk.
            Some(_) => load_config(&config_path).map_or(config, |(fresh, _)| fresh),
            None => config,
        }
    } else {
        config
    };

    // Held to the end of this function, which is the end of logging: dropped on
    // every return and on a panic that unwinds through here, and dropping it
    // drains the queue to disk. `let _ =` would drop it on this line.
    let file_log = init_tracing(args.headless);

    // A misspelled key is kept in the file and has no effect, so say so once
    // at boot. Never fatal: unmodelled keys are kept on purpose (see
    // `config_keys`).
    if let Some(line) = config_keys::boot_line(&config_keys::unknown_keys(
        &serde_json::Value::Object(config.extra.clone()),
    )) {
        tracing::warn!(target: "tcr", "{line}");
    }

    // Auto-migrating `throttle` in memory (see `config::load`) is only half the
    // fix Gil asked for — "our code should auto migrate" means the file itself
    // stops carrying the stale key, so the operator is never re-warned on every
    // boot and a later `tcr accounts`/`tcr status` sees a config that already
    // reflects the split. This is deliberately the ONLY place that persists a
    // migration: read-only CLI verbs migrate in memory and stay read-only. See
    // `migration_persist_target` for what gates the write.
    if let Some(path) = migration_persist_target(&config, &persist_path) {
        match config::save(path, &config) {
            Ok(()) => {
                let msg = format!(
                    "migrated the legacy `throttle` config key to `accountThrottle`/\
                     `fleetThrottle` and rewrote {} — this should only happen once",
                    path.display()
                );
                tracing::warn!(target: "tcr", "{msg}");
                eprintln!("[tcr] {msg}");
            }
            Err(err) => {
                tracing::warn!(target: "tcr",
                    error = %err,
                    "auto-migrated the legacy `throttle` key in memory, but failed to \
                     persist the migration to disk; it will re-run at every future boot \
                     until this is fixed"
                );
                eprintln!(
                    "[tcr] warning: could not persist the auto-migrated config to {}: {err}",
                    path.display()
                );
            }
        }
    }

    // Spelled out rather than built from `ServeOptions::new`, which is
    // deliberately inert: writing the config back, owning the shared pin cache
    // and signalling whatever holds the port are all things only the BINARY may
    // do, so the binary is the place they are written down.
    let options = server::ServeOptions {
        config,
        persist_path,
        port: args.port,
        incumbent: if args.replace {
            server::IncumbentPolicy::never_signal()
        } else {
            server::IncumbentPolicy::report_and_stand_down()
        },
        affinity_path: Some(affinity::default_path()),
        // The Sessions/Tools panel cache, a binary-only side effect exactly like the
        // pin cache above and for the same reason: one shared file, one writer.
        wire_sessions_path: Some(tcr::stores::session_wire_persist::default_path()),
        // The shared usage ledger, a binary-only side effect exactly like the
        // pin cache above and for the same reason: one directory, one writer.
        usage_dir: Some(tcr::records::usage::default_dir()),
        tls: server::TlsSetup::Load,
        // This is a standalone `tcr` process, stated rather than sniffed from
        // `argv[0]`: the owner file is what makes a proxy identifiable when its
        // process name is NOT `tcr` (see `tcr::stores::singleton`), so the value
        // has to come from the caller that knows.
        host: singleton::ProxyHost::Cli,
        // Claim the port for the next `tcr` (and for `tcr login`) to read. A
        // binary-only side effect, like the pin cache above: it is a shared
        // directory, so a library caller must opt in with its own.
        //
        // The DIRECTORY, not a path: `serve` names the file after the port it
        // actually binds. Re-deriving `--port unwrap_or config.proxy.port` here to
        // build the name would be a second copy of a resolution rule that lives in
        // `serve`, and the two silently disagreeing means a claim named for a port
        // this process never bound — which every reader looks straight past.
        owner_dir: Some(singleton::default_owner_dir()),
        handover,
    };

    let handle = match server::serve(options).await? {
        server::ServeOutcome::Started(handle) => handle,
        // A binary may exit; the library returned this as a value.
        server::ServeOutcome::StoodDown(stand_down) => {
            // `--replace` serves with the never-signal policy, which stands down
            // without reaching `takeover_port`, the only other place that prints
            // the marker line TcrBar reads to tell a stand-down from a crash.
            if args.replace {
                eprintln!(
                    "{}",
                    singleton::stand_down_notice(
                        stand_down.port,
                        singleton::Incumbent {
                            pid: stand_down.pid,
                            kind: stand_down.kind,
                        },
                    )
                );
            }
            drop(restart_endpoint);
            stand_down_exit(&stand_down, file_log)
        }
    };
    let bound = handle.addr();

    // Fan control: a binary-only side effect like the caches above, because
    // `fan.json` is one shared file and the helper takes one holder. Started
    // after the bind, so a process that stood down never dials the helper.
    match tcr::stores::fan::start(tcr::base::paths::cache_dir().fan_file()) {
        Ok(fan) => handle.manager().install_fan(fan),
        Err(error) => {
            tracing::warn!(target: "tcr", %error, "fan control: could not start its thread; fans stay automatic");
        }
    }

    let mut handle = handle;
    let mut successor = None;
    let mut replacement_deadline = None;
    use tokio::signal::unix::{SignalKind, signal};
    let mut sigterm = signal(SignalKind::terminate()).ok();
    if sigterm.is_none() {
        tracing::warn!(target: "tcr", "could not install SIGTERM handler");
    }
    loop {
        if args.headless {
            // Installed BEFORE the "listening" line below, not after: that line is
            // what a caller (or `tests/headless_sigterm.rs`) waits on before it
            // treats this process as ready to signal. A handler installed after
            // the print leaves a real window, small, but wide enough for a
            // heavily loaded box to hit it, where a SIGTERM sent the instant the
            // line appears finds no handler yet and the default disposition kills
            // the process immediately, skipping `handle.shutdown()` below
            // entirely: no drain, no final session->account pin flush, no
            // owner-file removal. Registering first closes that window: by the
            // time the readiness line is on the wire, the handler is already
            // armed.
            //
            // TcrBar always launches this process with `--headless` and stops it
            // with `process.terminate()` — a SIGTERM, not Ctrl-C
            // (`apps/macos/.../ServerController.swift`). Mirror the TUI branch's
            // SIGTERM handling (below) so a supervised stop falls through to the
            // same shared cleanup instead of a hard kill.
            //
            // This cannot turn into an unkillable process: `handle.shutdown()`
            // is `shutdown_within(DEFAULT_SHUTDOWN_GRACE)` (5s, see
            // `server::DEFAULT_SHUTDOWN_GRACE`), documented as bounded — tasks
            // that miss the grace are aborted and counted in
            // `report.tasks_aborted`, warned about just below.
            tracing::info!(target: "tcr", "tcr listening on http://{bound} (headless)");
            // Block until Ctrl-C, SIGTERM, or the server task exits.
            let trigger = tokio::select! {
                _ = tokio::signal::ctrl_c() => ShutdownTrigger::CtrlC,
                () = async {
                    match sigterm.as_mut() {
                        Some(sig) => {
                            sig.recv().await;
                        }
                        None => std::future::pending::<()>().await,
                    }
                } => ShutdownTrigger::Sigterm,
                () = handle.serving_stopped() => ShutdownTrigger::ServingStopped,
                peer = tcr::stores::restart::accept(restart_endpoint.as_ref()) => {
                    successor = Some(peer);
                    ShutdownTrigger::Replaced
                },
            };
            tracing::info!(target: "tcr", "{}", trigger.shutdown_line());
        } else {
            // The TUI owns the foreground. Under raw mode Ctrl-C arrives as a keystroke,
            // so the loop (not a signal) handles it. But an EXTERNAL SIGTERM — e.g. the
            // singleton replacing us on the port — would otherwise terminate the process
            // with the terminal still in raw + alternate-screen mode, wrecking the
            // caller's shell. Race the TUI against SIGTERM: on the signal the `select`
            // drops the TUI future, and dropping it runs `TerminalGuard`'s destructor,
            // which restores the terminal (the same restore path as a clean quit or a
            // panic) BEFORE the shutdown/flush below and any SIGKILL fallback.
            let tui_fut = tui::run(Arc::clone(handle.manager()));
            tokio::pin!(tui_fut);
            tokio::select! {
                peer = tcr::stores::restart::accept(restart_endpoint.as_ref()) => {
                    successor = Some(peer);
                }

                res = &mut tui_fut => {
                    if let Err(err) = res {
                        tracing::error!(target: "tcr", error = %err, "tui error");
                    }
                }
                () = async {
                    match sigterm.as_mut() {
                        Some(sig) => {
                            sig.recv().await;
                        }
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    tracing::info!(target: "tcr", "SIGTERM received; restoring terminal and shutting down");
                }
            }
        }

        let Some(peer) = successor.take() else {
            break;
        };
        let deadline = tokio::time::Instant::now() + tcr::stores::restart::drain_ceiling();
        let result = tokio::select! {
            result = handle.handoff(&peer) => result,
            _ = tokio::signal::ctrl_c() => break,
            () = async { match sigterm.as_mut() {
                Some(signal) => { signal.recv().await; },
                None => std::future::pending::<()>().await,
            }} => break,
            () = tokio::time::sleep_until(deadline) => {
                tracing::warn!(target: "tcr", "replacement handoff deadline reached; shutting down"); break;
            }
        };
        if let Err(error) = result {
            if !handle.manager().mutation_is_released() {
                tracing::warn!(target: "tcr", %error, "listener handoff failed before send; incumbent keeps serving");
                continue;
            }
            tracing::warn!(target: "tcr", %error, "listener handoff failed; draining before shutdown");
        }
        successor = Some(peer);
        replacement_deadline = Some(deadline);
        break;
    }

    if let Some(peer) = successor {
        // One budget includes the handshake and draining. A failed or interrupted
        // handshake still falls through to shutdown and the usage-ledger flush.
        let replacement = async {
            tracing::info!(target: "tcr", "listener handed over; draining accepted connections");
            handle.drain_connections().await;
            tracing::info!(target: "tcr", "accepted connections drained");
        };
        tokio::select! {
            () = replacement => {},
            _ = tokio::signal::ctrl_c() => tracing::info!(target: "tcr", "shutdown signal received during drain"),
            () = async {
                match sigterm.as_mut() {
                    Some(sig) => { sig.recv().await; }
                    None => std::future::pending::<()>().await,
                }
            } => tracing::info!(target: "tcr", "SIGTERM received during drain; shutting down"),
            () = tokio::time::sleep_until(replacement_deadline.unwrap_or_else(tokio::time::Instant::now)) => {
                tracing::warn!(target: "tcr", "replacement drain deadline reached; shutting down");
            }
        }
        // Not the close the successor waits for: the handle holds its own copy
        // of the channel until `shutdown` below has drained the usage ledger.
        drop(peer);
    }
    drop(restart_endpoint);

    // Stop serving, then flush the config and the affinity pins. The whole
    // sequence — including the final pin write a clean shutdown owes the next
    // boot — lives in `ServerHandle::shutdown`, so an embedder gets it too.
    // Bounded there, so a wedged filesystem cannot turn quitting the TUI into a
    // hang with the terminal already restored and nothing left serving.
    let report = handle.shutdown().await;
    if report.tasks_aborted > 0 {
        tracing::warn!(target: "tcr",
            aborted = report.tasks_aborted,
            "background task(s) did not stop within the shutdown grace and were aborted"
        );
    }
    Ok(())
}

/// Which event broke the headless `select!` in [`run_server`] out of its
/// wait, so it can fall through to the shared `handle.shutdown()` below.
/// Kept as a plain enum the `select!` arms produce, rather than matching
/// inline in each arm, so the mapping from event to log message lives in
/// one `match`.
///
/// The coverage claim — that SIGTERM is actually one of these triggers —
/// is NOT proven by a list living beside this enum: a hand-written list
/// can drift from the `select!` arms with nothing to notice, which is
/// exactly the shape of gate that cannot fail for the defect it exists to
/// catch. It is proven by `tests/headless_sigterm.rs`, which spawns the
/// real binary, sends it a real SIGTERM, and asserts on the externally
/// observable effects of a graceful shutdown (the log line and the
/// port-owner claim being withdrawn) — deleting the SIGTERM arm here makes
/// that test fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShutdownTrigger {
    CtrlC,
    Sigterm,
    ServingStopped,
    Replaced,
}

impl ShutdownTrigger {
    /// The line this trigger logs on the way out. Every variant returns one.
    ///
    /// `ServingStopped` used to return nothing: its match arm was `=> {}`, and
    /// the only thing after the match is a `warn!` that fires solely when tasks
    /// had to be aborted. So a shutdown down that path left the durable log
    /// unable to say the process had stopped, let alone why. This repo counts
    /// boots with `rg 'server started' <cache dir>/logs/*` precisely
    /// because a restart is the most expensive event in the system; a stop with
    /// no matching line makes that count unreadable.
    ///
    /// Returned rather than logged inline so every variant is forced to have
    /// one by the type system, and so `every_shutdown_trigger_logs_a_line` can
    /// check them without a running server.
    const fn shutdown_line(self) -> &'static str {
        match self {
            Self::CtrlC => "shutdown signal received",
            Self::Sigterm => "SIGTERM received; shutting down",
            Self::ServingStopped => "serving stopped on its own; shutting down",
            Self::Replaced => "replacement requested; handing over the listener",
        }
    }
}

/// How to recover from a WEDGED incumbent — the half of the not-answering warning
/// that depends on which proxy is holding the port.
///
/// `--replace` is the recovery for a proxy this process may signal. It is not one
/// for a [`singleton::ProxyKind::TcrEmbedded`] incumbent, and offering it there is
/// worse than offering nothing: `takeover_decision` refuses that kind on every
/// path, so the operator runs the suggested command, sees the same stand-down, and
/// the advice that DOES work — quitting the host application — was never printed.
/// An instruction that cannot succeed is a bug even when the code behind it is
/// correct.
const fn wedged_incumbent_recovery(kind: singleton::ProxyKind) -> &'static str {
    match kind {
        singleton::ProxyKind::TcrEmbedded => {
            "`tcr --replace` cannot take this one over: the pid belongs to the host application \
             serving the proxy in-process, and signalling it would kill the app without its \
             normal shutdown, losing the session→account pin map. Quit the host application and \
             start it again to recover a wedged embedded proxy."
        }
        singleton::ProxyKind::Tcr => {
            concat!(
                "Stop the proxy explicitly, or use `tcr --replace` if it runs tcr ",
                tcr::handoff_since!(),
                " or newer and its listener handoff endpoint still answers."
            )
        }
    }
}

/// Print the stand-down diagnosis and exit with the code it earned. Never returns.
///
/// This is the half of the stand-down a *library* must not do, which is why
/// [`server::serve`] hands the facts back as a [`server::StandDown`] instead.
/// The wording is a cross-language contract: TcrBar scans this stderr, and
/// `ServerController.StandDownExit` switches on the code.
///
/// Takes the log's owner by value because `std::process::exit` runs no
/// destructor: whatever is still in the writer's queue is closed onto disk
/// first, or it is lost.
fn stand_down_exit(
    stand_down: &server::StandDown,
    file_log: Option<tcr::stores::file_log::FileLog>,
) -> ! {
    // Standing down is cheap and correct, but silent success here would mean
    // `cargo build && tcr` exits 0 with the OLD build still serving — say which
    // build actually holds the port before we go.
    eprintln!("{}", stand_down.report.line);
    if let live_client::Liveness::Silent { why } = &stand_down.probe.liveness {
        let port = stand_down.port;
        let pid = stand_down.pid;
        let recovery = wedged_incumbent_recovery(stand_down.kind);
        eprintln!(
            "[tcr] WARNING incumbent-not-answering: port={port} pid={pid} probe={why:?} — the \
             process holding :{port} did not respond, so standing down leaves NOTHING serving \
             on it. {recovery}"
        );
    }
    if let Some(file_log) = file_log {
        file_log.close();
    }
    std::process::exit(stand_down_exit_code(
        &stand_down.probe.liveness,
        stand_down.report.verdict,
    ));
}

/// Where (if anywhere) `run_server` should persist an in-memory `throttle`
/// migration: only when `load` actually migrated something, a file path exists
/// to write it to, and no account is quarantined. Pulled out as a pure
/// function so the decision is unit-testable without booting a real server —
/// see the `migration_persist_target_*` tests below.
///
/// The quarantine gate mirrors `cli::load_for_edit`: writing back a `Config`
/// while an account is quarantined would serialize over its raw JSON
/// (`importFrom` pointer included) and drop it permanently, so this skips the
/// write and leaves the on-disk file for a human to fix — same hazard, same
/// guard.
fn migration_persist_target<'a>(
    config: &Config,
    persist_path: &'a Option<PathBuf>,
) -> Option<&'a Path> {
    if config.should_save_throttle_migration() {
        persist_path.as_deref()
    } else {
        None
    }
}

/// Load the config, deciding what may be written back:
/// - missing file → [`config::load_or_init`] writes the defaults out before
///   returning them, so the server's own first boot leaves a real file on disk
///   (what every other verb now also sees) instead of running on a default the
///   next `tcr status` cannot find;
/// - corrupt/unreadable existing file → **refuse to start**. This used to fall
///   back to in-memory defaults (a zero-account fleet) and boot anyway — a
///   proxy that binds its port and answers every request with 429 while
///   looking alive, which is worse than refusing outright: a dead proxy that
///   won't start is obvious, and this one was not (see `config::load`'s
///   doc-comment on the migration this replaced). A missing file is a
///   legitimate first run and still boots on defaults; a file that exists and
///   fails to parse is now the operator's problem to fix, not something this
///   binary papers over.
pub(super) fn load_config(path: &Path) -> anyhow::Result<(Config, Option<PathBuf>)> {
    match config::load_or_init(path) {
        Ok((config, _created)) => Ok((config, Some(path.to_path_buf()))),
        Err(ConfigError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {
            // `load_or_init` only hands back NotFound when the CREATE itself
            // could not find its way (an unwritable/absent parent that
            // `create_dir_all` refused); the read's own NotFound is handled
            // there. Booting on defaults keeps that case serving, exactly as a
            // missing file used to.
            eprintln!(
                "[tcr] no config at {} and it could not be created ({err}) — starting with defaults",
                path.display()
            );
            Ok((parse_default_config()?, Some(path.to_path_buf())))
        }
        Err(err) => anyhow::bail!(
            "config at {} is unreadable/corrupt: {err} — refusing to start rather than \
             serve an empty fleet; fix the file and restart",
            path.display()
        ),
    }
}

/// A default config with every serde default applied (correct `upstream` and
/// `switchThreshold`, empty accounts) — parsing `{}` reuses the config's own
/// `#[serde(default)]` wiring instead of a hand-rolled `Default`.
fn parse_default_config() -> anyhow::Result<Config> {
    serde_json::from_str("{}").context("build the default config")
}

#[cfg(test)]
pub(super) fn default_config() -> Config {
    parse_default_config().expect("an empty JSON object is always a valid default config")
}
