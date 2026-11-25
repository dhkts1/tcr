//! Tests of the server's logging: the owner-only log directory, both headless sinks, and pruning.

use super::*;
use std::os::unix::fs::PermissionsExt as _;
use tcr::stores::affinity;

/// A unique, test-only log directory keyed by pid + nanosecond timestamp.
/// Never the real cache directory's `logs/` — tests must not touch the
/// live proxy's cache directory (`session-affinity.json` lives one level up
/// and is being written by a running process).
fn unique_log_dir(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "tcr-test-{tag}-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or_default(|d| d.as_nanos())
    ))
}

/// The rotating log directory (account emails + request paths inside it)
/// must be created owner-only. `tracing_appender` has no per-file
/// `.mode()` hook (verified against its 0.2.5 source — `create_writer` in
/// `rolling.rs` opens with `OpenOptions::append(true).create(true)` only),
/// so confidentiality is enforced on the directory, not the file: `0700`
/// blocks traversal into the directory for everyone but the owner, even
/// though the files landing inside it carry whatever mode the process
/// umask gives a freshly `OpenOptions::create`d file (commonly `0644` —
/// world-readable in their *own* bits, but unreachable because nothing
/// outside the owner can resolve a path through a `0700` parent).
#[test]
fn log_directory_is_created_owner_only() {
    let dir = unique_log_dir("dirmode");
    // Goes through the production opener: re-implementing the mode here
    // would assert 0o700 == 0o700 and pass however `open_log_appender`
    // drifts.
    let _appender =
        open_log_appender(&dir, tcr::base::paths::LOG_PREFIX).expect("open temp log dir");
    let dir_mode = std::fs::metadata(&dir)
        .expect("stat temp log dir")
        .permissions()
        .mode()
        & 0o777;
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(dir_mode, 0o700, "log directory must be owner-only (0700)");
}

/// `DirBuilder::mode` only sets the mode of directories it *creates* — a
/// directory that already exists (operator `mkdir -p`, a `tar` restore
/// without `-p`, a baked container path) is left exactly as it was found.
/// `open_log_appender` must re-assert `0700` on an already-existing
/// directory, not merely request it at creation time, or a pre-existing
/// `0755` `logs/` silently ships every rotated file world-readable.
#[test]
fn log_directory_is_re_asserted_owner_only_when_it_already_exists() {
    use std::os::unix::fs::DirBuilderExt as _;
    let dir = unique_log_dir("dirmode-preexisting");
    std::fs::DirBuilder::new()
        .mode(0o755)
        .recursive(true)
        .create(&dir)
        .expect("pre-create the dir at 0755, simulating an external mkdir/restore");
    let preexisting_mode = std::fs::metadata(&dir)
        .expect("stat pre-created dir")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        preexisting_mode, 0o755,
        "test setup sanity: the pre-created dir must actually be 0755"
    );

    let _appender =
        open_log_appender(&dir, tcr::base::paths::LOG_PREFIX).expect("open pre-existing log dir");
    let dir_mode = std::fs::metadata(&dir)
        .expect("stat temp log dir")
        .permissions()
        .mode()
        & 0o777;
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(
        dir_mode, 0o700,
        "a pre-existing, wrongly-permissioned log directory must be re-asserted to 0700"
    );
}

// A `MakeWriter` that keeps what was written, so a test can inspect the
// stdout sink without capturing the process's real stdout.
use tcr::test_support::log_capture::SharedBuf;

/// Reads back whatever `open_log_appender` actually wrote inside `dir` —
/// there is exactly one matching file after a single, unrotated write.
/// This is the "it writes", not merely "it initialised", proof: a dropped
/// `WorkerGuard` (see `headless_subscriber`'s doc-comment) would make
/// `init_tracing`-style construction succeed while nothing ever lands here.
///
/// `prefix` is the one the appender was opened with: the resolved cache
/// directory's own, never a fixed name.
fn read_the_one_log_file(dir: &Path, prefix: &str) -> String {
    let mut matches: Vec<_> = std::fs::read_dir(dir)
        .expect("read temp log dir")
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with(prefix))
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "expected exactly one log file in {dir:?}, found {matches:?}"
    );
    std::fs::read_to_string(matches.pop().unwrap().path()).expect("read the log file")
}

/// HEADLESS MUST REACH DISK. TcrBar spawns `tcr server --headless` and throws
/// the child's stdout away, so a stdout-only headless subscriber destroys
/// 100% of the proxy's logs — which is exactly why a restart loop could not
/// be diagnosed. Both sinks are asserted: dropping either one is a
/// regression, and asserting only the file would let stdout silently die for
/// everyone running it in a terminal.
#[test]
fn headless_logging_reaches_both_the_file_and_stdout() {
    let dir = unique_log_dir("headless-test");
    let prefix = tcr::base::paths::LOG_PREFIX;
    let appender = open_log_appender(&dir, prefix).expect("open temp log dir");
    let (queue, file_log) = tcr::stores::file_log::start(appender);
    let stdout = SharedBuf::default();
    let marker = format!("headless-sink-probe-{}", std::process::id());

    let subscriber = headless_subscriber(
        tracing_subscriber::EnvFilter::new("info"),
        Some(queue),
        Some(stdout.clone()),
        false,
    );
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!("{marker}");
    });
    // The writer is still running: sync, not close, so this proves the line
    // reached disk while logging continues.
    assert!(
        file_log.sync(std::time::Duration::from_secs(10)),
        "the log writer never reached the barrier"
    );

    let on_disk = read_the_one_log_file(&dir, prefix);
    std::fs::remove_dir_all(&dir).ok();
    assert!(
        on_disk.contains(&marker),
        "headless event never reached the log file; file held: {on_disk:?}"
    );
    assert!(
        !on_disk.contains('\u{1b}'),
        "the log file must be non-ANSI; escape codes read back as garbage"
    );
    assert!(
        stdout.contents().contains(&marker),
        "headless event never reached stdout; stdout held: {:?}",
        stdout.contents()
    );
}

/// Redirected stdout carries no escape codes.
///
/// The library default would: `Layer::default` enables colour whenever
/// `NO_COLOR` is unset, whatever the sink is, so a `tcr --headless >
/// run.log` on a machine without `NO_COLOR` wrote a log that no `rg` of a
/// field name could match. Asserted on both sinks, and against the same
/// sink with colour ON, so a `stdout_ansi` argument that is quietly
/// ignored fails here rather than on somebody's CI runner.
#[test]
fn headless_stdout_is_plain_when_it_is_not_a_terminal() {
    let plain = SharedBuf::default();
    let marker = format!("headless-ansi-probe-{}", std::process::id());

    let subscriber = headless_subscriber(
        tracing_subscriber::EnvFilter::new("info"),
        None,
        Some(plain.clone()),
        false,
    );
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(peer_listen = %"127.0.0.1:34865", "{marker}");
    });

    let written = plain.contents();
    assert!(
        !written.contains('\u{1b}'),
        "non-terminal stdout must hold no escape byte; held: {written:?}"
    );
    assert!(
        written.contains("peer_listen=127.0.0.1:34865"),
        "a reader must find the plain `key=value`; held: {written:?}"
    );

    // The control: the same call with colour on must differ, which is what
    // proves the assertion above measures the argument.
    let coloured = SharedBuf::default();
    let subscriber = headless_subscriber(
        tracing_subscriber::EnvFilter::new("info"),
        None,
        Some(coloured.clone()),
        true,
    );
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(peer_listen = %"127.0.0.1:34865", "{marker}");
    });
    assert!(
        coloured.contents().contains('\u{1b}'),
        "with colour on the same event carries escapes, so the plain case above is a real result"
    );
}

/// A log file that will not open must degrade to stdout-only, never take the
/// proxy down: logging is not worth the traffic it is observing.
#[test]
fn headless_logging_survives_an_unopenable_log_file() {
    let stdout = SharedBuf::default();
    let marker = format!("headless-degraded-probe-{}", std::process::id());

    let subscriber = headless_subscriber(
        tracing_subscriber::EnvFilter::new("info"),
        None,
        Some(stdout.clone()),
        false,
    );
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!("{marker}");
    });

    assert!(
        stdout.contents().contains(&marker),
        "with no log file, stdout must still receive every event"
    );
}

/// `init_tracing` must target the one shared, well-known cache directory,
/// not a private/generated path — so a human running the documented
/// `rg 'server started' <cache dir>/logs/*` recipe can always find
/// it. This is **not** because any code reads it back (a census —
/// `rg -n 'log_file_path|log_dir_path' src/ scripts/
/// apps/`, repo-relevant scope, not `src/` alone: a `src/`-only census once
/// missed `scripts/validate-cache.sh`'s own copy of the old fixed path —
/// found no programmatic reader anywhere outside this module; the previous
/// version of this test justified itself with "this is what `tcr status`
/// and every diagnostic already read", which was false and is not
/// repeated here).
///
/// Reads this process's real `HOME` to resolve the path, and writes nothing.
#[test]
fn the_log_directory_is_the_shared_cache_location() {
    let cache = tcr::base::paths::cache_dir();
    let (dir, prefix) = log_location();
    assert_eq!(dir, cache.root().join("logs"));
    assert_eq!(
        Some(cache.root()),
        affinity::default_path().parent(),
        "logs and the pin file share one cache directory"
    );
    assert_eq!(prefix, tcr::base::paths::LOG_PREFIX);
}

/// `max_log_files` pruning is real, not merely configured, AND it is the
/// production `open_log_appender`'s own hardcoded `max_log_files(5)`
/// being exercised — not a hand-rolled duplicate of the config, so a
/// regression that drops or weakens the production call is what this test
/// is sensitive to. Pre-creates 5 dated files (the `prefix.date` shape
/// `join_date()` produces for any non-`NEVER` rotation), each with a
/// distinct creation time, then calls the real production opener —
/// `prune_old_logs()` runs at *construction*, before the first new file is
/// created (verified against 0.2.5 source, `rolling.rs:615-617`), which is
/// what makes this deterministic: no wall-clock rotation boundary needs to
/// be crossed.
///
/// This is portable across the two CI targets, not merely convenient on
/// one. `prune_old_logs()` sorts by `metadata.created()` where the
/// platform supports it, but explicitly falls back to parsing the date out
/// of the filename itself when it does not (`rolling.rs:689-696`,
/// `parse_date_from_filename`) — and this test's embedded dates
/// (`2020-02-01` .. `2020-02-05`) sort in the same order as their real
/// creation timestamps, so the assertions hold identically whichever path
/// the pruner takes. Verified directly against this repo's own targets:
/// macOS (APFS) supports `created()`; the crate's own filename-parsing
/// fallback exists specifically because ext4/most Linux filesystems (this
/// repo's `ci` job runs `ubuntu-latest`) commonly return
/// `ErrorKind::Unsupported` for it — an attempt to break this test via
/// that path does not succeed, because the fallback is exercised, not
/// skipped.
#[test]
fn old_log_files_are_pruned_at_max_log_files() {
    assert_old_log_files_are_pruned(tcr::base::paths::LOG_PREFIX, "pruning");
}

fn assert_old_log_files_are_pruned(prefix: &str, tag: &str) {
    let dir = unique_log_dir(tag);
    std::fs::create_dir_all(&dir).expect("create temp log dir");

    // Five pre-existing dated files, oldest first, each with a distinct
    // creation time (the pruner sorts by `metadata.created()`).
    let oldest = "2020-02-01";
    let survivors = ["2020-02-02", "2020-02-03", "2020-02-04", "2020-02-05"];
    for date in std::iter::once(&oldest).chain(survivors.iter()) {
        std::fs::write(dir.join(format!("{prefix}.{date}")), b"old\n")
            .expect("write stale log file");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    // Production max_log_files(5): prune keeps (5 - 1) = 4 of the 5
    // existing files, then the appender creates one new file for today —
    // 5 total, with only the single oldest pre-existing file removed.
    let _appender = open_log_appender(&dir, prefix).expect("open temp log dir");

    let remaining: std::collections::BTreeSet<String> = std::fs::read_dir(&dir)
        .expect("read temp log dir")
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    std::fs::remove_dir_all(&dir).ok();

    assert!(
        remaining.len() >= 2,
        "at least 2 files must survive pruning: {remaining:?}"
    );
    assert!(
        !remaining.contains(&format!("{prefix}.{oldest}")),
        "the single oldest file must have been pruned: {remaining:?}"
    );
    for date in survivors {
        assert!(
            remaining.contains(&format!("{prefix}.{date}")),
            "pre-existing file {date} must survive pruning: {remaining:?}"
        );
    }
}
