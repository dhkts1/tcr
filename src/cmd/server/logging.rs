//! Where the binary logs: the shared log directory, the file appender, and the tracing subscriber both modes install.

use std::path::{Path, PathBuf};

/// The one shared, well-known, non-private directory both modes log into:
/// `logs/` in the cache directory, `~/.cache/tcr/logs/`
/// (`tcr::base::paths::cache_dir`), with the log file prefix `tcr.log`.
///
/// The path is fixed for **human discoverability** — so `rg 'server started'
/// ~/.cache/tcr/logs/*` always finds the right directory — not because any code
/// depends on the literal string. Nothing in this codebase opens this path
/// programmatically outside this module; see `open_log_appender` below and the
/// doc-comment on `the_log_directory_is_the_shared_cache_location` for the
/// census that established that.
///
/// This used to compute the cache base separately from
/// `affinity::default_path`, so a log-path bug could not brush the pin file.
/// It now shares `paths::cache_dir` with it on purpose: two resolutions of the
/// cache directory could disagree, and an install would split its cache across
/// two folders.
fn log_location() -> (PathBuf, &'static str) {
    (
        tcr::base::paths::cache_dir().log_dir(),
        tcr::base::paths::LOG_PREFIX,
    )
}

/// Open the durable, rotating log directory, injectable for tests so they never
/// touch the real cache directory's `logs/` — pass a unique temp dir instead
/// of routing through [`log_location`]. `prefix` is the file name prefix
/// rotation and pruning key on, and has to match the directory's own.
///
/// `tracing_appender::rolling` has no per-file `.mode()` hook (checked against
/// its 0.2.5 source: `create_writer` in its `rolling.rs` opens with
/// `OpenOptions::append(true).create(true)` only), so a rotating file cannot be
/// made owner-only the way the old single file was. Confidentiality is enforced
/// on the *directory* instead, and it is load-bearing on Linux CI targets, not
/// belt-and-braces: the parent cache directory (`~/.cache/tcr/`) is `0755` (protects its
/// existing contents by *file* mode — `session-affinity.json` is `0600`), so
/// without an owner-only `logs/` subdirectory, files landing at the crate's
/// default `0644` would be world-readable on any multi-user box. The mode is
/// requested atomically at directory-creation time via `DirBuilder::mode`, not
/// create-then-`chmod` — the latter leaves a TOCTOU window where the directory
/// is briefly world-traversable while it starts to hold sensitive log content.
///
/// The 0700 mode is applied at directory creation and re-asserted once at
/// process startup (below). It is **not** a standing invariant:
/// `tracing_appender`'s own fallback (`rolling.rs:795`) recreates the
/// directory with `create_dir_all` and no mode if it is removed externally —
/// at construction and at every rotation — so a `logs/` deleted mid-run
/// silently returns at 0755 for the life of the process. This function closes
/// the pre-existing-directory case (a stale `mkdir -p`, a `tar` restore
/// without `-p`, a baked container path); it cannot reach that crate-internal
/// recreation path, which is not this function's to fix.
///
/// The mode is checked on the **resolved** target, not the path itself:
/// `std::fs::metadata`/`set_permissions` both follow symlinks, and
/// `DirBuilder` succeeds against an existing symlink-to-directory, so a
/// symlinked `logs/` is validated by where it points, not by the link. That
/// is deliberate, not an oversight: pointing the log directory at another
/// volume is a legitimate operator setup, and refusing to start over a
/// symlink would break a working configuration to close a hole that is not a
/// privilege boundary anyway — planting such a symlink first requires write
/// access to the 0755, user-owned cache directory parent, i.e. the
/// user or root, who already has more reach than this would buy them.
///
/// `.recursive(true)` creates every missing path component, parents included,
/// **carrying the same `0700` mode** — this is not confined to `logs/` itself.
/// On a fresh install or in a container where the cache directory
/// (`~/.cache/tcr/`, or even `~/.cache/`) does not yet exist, this call creates it at `0700`, changing a
/// directory shared with every other application on the machine. That is
/// invisible on a dev box where both already exist at `0755`, but it is real,
/// measured behaviour of `DirBuilder::recursive`, not a hypothetical.
///
/// Rotation is `DAILY` with `max_log_files(5)`: measured against the live log
/// (2026-08-08) at ~13.5 MiB/day, this bounds this directory's steady-state
/// disk use to roughly 65-70 MiB instead of genuinely unbounded growth. That
/// is a wall-clock bound, not a byte-size bound: a single unusually
/// verbose day can still exceed the per-file average before the next
/// rotation, so this is a soft cap, not a hard one. Rotation also rolls on
/// `Rotation::DAILY`'s UTC clock (`rolling.rs` uses `OffsetDateTime::now_utc`),
/// not local midnight — for the stated goal of human discoverability, that
/// means the daily boundary lands at 03:00 in Asia/Jerusalem, not midnight.
fn open_log_appender(
    log_dir: &Path,
    prefix: &str,
) -> std::io::Result<tracing_appender::rolling::RollingFileAppender> {
    use std::os::unix::fs::DirBuilderExt as _;
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(log_dir)?;

    // `DirBuilder::mode` only governs directories it *creates* — a directory
    // that already existed (operator `mkdir -p`, a `tar` restore without
    // `-p`, a baked container path) is untouched by the call above and could
    // be sitting at 0755 or worse. Re-assert here so every process start
    // closes that window, not just a genuinely-fresh directory.
    let mode = std::fs::metadata(log_dir)?.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        std::fs::set_permissions(log_dir, std::fs::Permissions::from_mode(0o700))?;
        let confirmed = std::fs::metadata(log_dir)?.permissions().mode() & 0o777;
        if confirmed & 0o077 != 0 {
            return Err(std::io::Error::other(format!(
                "log directory {} is not owner-only after chmod (mode {confirmed:#o}); \
                 refusing to log account emails into a world-readable directory",
                log_dir.display()
            )));
        }
    }

    tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix(prefix)
        .max_log_files(5)
        .build(log_dir)
        .map_err(std::io::Error::other)
}

/// Build the headless subscriber: every event goes to **both** `stdout_sink` and
/// (when it could be opened) the durable log file.
///
/// Headless used to be stdout-only, which meant its logs were destroyed outright
/// whenever a supervisor discarded the child's stdout — as TcrBar does
/// (`ServerController.swift`, `standardOutput = FileHandle.nullDevice`, and older
/// builds hand it an undrained pipe). A crashing proxy then left no evidence of
/// why. Stdout is kept as well, because someone running `tcr server --headless`
/// in a terminal expects output, and launchd-style supervisors capture it.
///
/// `file` is an `Option` on purpose: a logging failure must never take the proxy
/// down, so a log that will not open degrades to stdout-only.
///
/// `file` is the queue of [`tcr::stores::file_log`]'s writer thread, not the appender:
/// the emitting thread formats its line and queues it, and one thread writes.
/// The synchronous `Mutex<RollingFileAppender>` this replaced was the proxy's
/// top frame under load (`docs/performance.md`). The queue comes with a
/// `WorkerGuard` whose drop silently stops every later line from reaching
/// disk, so this function never sees it: [`init_tracing`] returns it, wrapped
/// in a [`tcr::stores::file_log::FileLog`], for `run_server` to hold for the process's
/// life and close on every way out. That module's doc lists each one.
///
/// `stdout_sink` is an `Option` because stdout is skipped when it is the null
/// device, as TcrBar's is: formatting and writing every line a second time,
/// under `Stdout`'s own lock, for a sink that discards it was 3 to 7% of the
/// proxy's samples in the load test. A terminal, a pipe or a file keeps its
/// copy, written synchronously so a human or a supervisor sees each line when
/// it happens.
/// `stdout_ansi` is the caller's decision, made once, because the library's
/// own default is wrong here: `tracing_subscriber::fmt::Layer::default` turns
/// colour on whenever `NO_COLOR` is unset and never asks whether the sink is a
/// terminal, so a redirected `tcr --headless > run.log` fills the file with
/// escape codes. That is a bug by itself (a log nobody can grep), and it broke
/// `tests/peer/peer_e2e.rs` on CI, where `NO_COLOR` is unset and the children's
/// boot logs are files: every field name arrived as
/// `\e[3mpeer_listen\e[0m\e[2m=\e[0m`.
fn headless_subscriber<W>(
    filter: tracing_subscriber::EnvFilter,
    file: Option<tracing_appender::non_blocking::NonBlocking>,
    stdout_sink: Option<W>,
    stdout_ansi: bool,
) -> impl tracing::Subscriber + Send + Sync
where
    W: for<'w> tracing_subscriber::fmt::MakeWriter<'w> + Send + Sync + 'static,
{
    use tracing_subscriber::layer::SubscriberExt as _;
    // The file sink is non-ANSI: escape codes in a log read back as garbage.
    let file_layer = file.map(|file| {
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(file)
    });
    let stdout_layer = stdout_sink.map(|sink| {
        tracing_subscriber::fmt::layer()
            .with_ansi(stdout_ansi)
            .with_writer(sink)
    });
    tracing_subscriber::registry()
        .with(filter)
        .with(stdout_layer)
        .with(file_layer)
}

/// Initialise tracing. Headless logs to stdout *and* the durable log file; the
/// TUI logs only to the file, so events never corrupt the alternate screen.
/// Either way, a log file that will not open is a warning, never a failure.
///
/// Returns the owner of the file's writer thread, `None` when no file opened.
/// The caller holds it for as long as anything should reach the file and
/// closes it before any exit that runs no destructor: dropping it early stops
/// every later line from reaching disk, silently (see [`tcr::stores::file_log`]).
/// The TUI writes through the same queue for the same reason as headless: its
/// file was behind the same lock, on the same tokio workers.
/// The log filter when `RUST_LOG` is unset: `info`, and in a build with tcr's
/// own tailnet node, `warn` for the `tailscale` crate and its `ts_*` and
/// `kameo` dependencies, whose info lines would otherwise reach tcr's log on
/// every node build and reconnect. A target directive matches by prefix, so
/// `ts_` covers every `ts_*` crate.
#[cfg(feature = "tailnet-node")]
const DEFAULT_LOG_FILTER: &str = "info,tailscale=warn,ts_=warn,kameo=warn";

/// The log filter when `RUST_LOG` is unset.
#[cfg(not(feature = "tailnet-node"))]
const DEFAULT_LOG_FILTER: &str = "info";

pub(super) fn init_tracing(headless: bool) -> Option<tcr::stores::file_log::FileLog> {
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::util::SubscriberInitExt as _;
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG_FILTER));
    let (log_dir, log_prefix) = log_location();
    let (queue, file_log) = match open_log_appender(&log_dir, log_prefix) {
        Ok(file) => {
            let (queue, file_log) = tcr::stores::file_log::start(file);
            // Before the subscriber exists, so no line is ever queued that a
            // panic would leave behind.
            tcr::stores::file_log::install_panic_flush(file_log.flusher());
            (Some(queue), Some(file_log))
        }
        Err(err) => {
            eprintln!(
                "[tcr] could not open log directory {}: {err}",
                log_dir.display()
            );
            (None, None)
        }
    };
    if headless {
        let stdout = std::io::stdout();
        // A null-device stdout discards every line, so it gets no layer. With no
        // log file it keeps one anyway: a proxy logging nowhere at all should
        // fail where a test of stdout can see it.
        let stdout_sink = (queue.is_none()
            || !tcr::stores::file_log::is_null_device(std::os::fd::AsFd::as_fd(&stdout)))
        .then_some(std::io::stdout);
        // Colour only for a human at a terminal. Piped or redirected stdout is
        // something a program or a test will read back, and `NO_COLOR` is still
        // honoured on top: see `headless_subscriber`.
        let ansi = std::io::IsTerminal::is_terminal(&stdout)
            && std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty());
        headless_subscriber(filter, queue, stdout_sink, ansi).init();
        return file_log;
    }
    // No file? Already warned above. The TUI cannot fall back to stdout without
    // corrupting the alternate screen, so it runs without tracing rather than
    // refusing to start.
    if let Some(queue) = queue {
        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_env_filter(filter)
            .with_writer(queue)
            .init();
    }
    file_log
}

#[cfg(test)]
mod tests;
