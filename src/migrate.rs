//! `tcr migrate`: move an install's `teamclaude` config file and cache
//! directory to the `tcr` names.
//!
//! [`crate::paths`] already reads either name (new if it exists, else old if
//! it exists, else new), so nothing breaks while the old names stay. This is
//! the step that retires them. It never belongs to a server start: TcrBar
//! runs it at launch, before it starts its own server, whenever old names are
//! still there, and `tcr update` runs it after a successful install or rebuild
//! ([`after_update`]).
//!
//! **It refuses while a tcr server is running.** A server resolved its paths at
//! boot and keeps writing to them, so moving the config under it splits the
//! config in two: the server's next write recreates `teamclaude.json` beside
//! the moved `tcr.json`, and from then on the new file wins. Detection is
//! [`crate::singleton::live_proxy_server`] on the port the config names, the
//! same detection `tcr login` uses to refuse.
//!
//! **Per unit, one rule.** The units are the ones [`crate::paths`] decides:
//! the config file, and the cache directory as a whole.
//! - Only the old one exists: `rename(2)` it to the new name. Same parent
//!   directory, so the move is atomic: there is never half a unit.
//! - Both exist: nothing moves. The new one is already the one in use.
//! - Only the new one exists, or neither: nothing to do.
//! - The rename fails: the old unit stays where it is and is still used,
//!   because the read fallback finds it. Nothing is ever copied and deleted.
//!
//! Inside the cache directory the rotating logs keep their old prefix after
//! the directory moves, and `tracing_appender` prunes by prefix, so they are
//! renamed to the new prefix too ([`rename_old_logs`]).
//!
//! The certificate files are not touched. tcr's own CA and leaf already have
//! `tcr-` names, and the `teamclaude-` ones belong to the Node proxy, whose
//! users point `NODE_EXTRA_CA_CERTS` at them ([`crate::legacy_names`]).

use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::legacy_names;
use crate::paths::{self, Roots};
use crate::singleton::{self, Incumbent, ProxyKind};

/// Which of the two units a line of the report is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Unit {
    /// `~/.config/<name>.json`.
    Config,
    /// `<cache base>/<name>/`, with everything inside it.
    Cache,
}

impl Unit {
    const fn label(self) -> &'static str {
        match self {
            Self::Config => "config file",
            Self::Cache => "cache directory",
        }
    }
}

/// What happened to one unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum UnitOutcome {
    /// Only the old one existed, and it was renamed to the new name.
    Moved { from: PathBuf, to: PathBuf },
    /// Both exist. Nothing moved; the new one is used.
    BothExist { old: PathBuf, new: PathBuf },
    /// Only the new one exists: an earlier run, or a fresh install.
    AlreadyNew { path: PathBuf },
    /// Neither exists. The first write creates the new one.
    Neither { new: PathBuf },
    /// The rename failed. The old one is still in place and still used.
    Failed {
        from: PathBuf,
        to: PathBuf,
        error: String,
    },
}

/// One unit's line in the report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnitReport {
    pub unit: Unit,
    #[serde(flatten)]
    pub outcome: UnitOutcome,
}

/// What happened to one old-prefix log file inside the cache directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum LogOutcome {
    /// Renamed to the new prefix, same suffix.
    Renamed { from: PathBuf, to: PathBuf },
    /// A file with the new name is already there, so this one was left alone
    /// rather than written over.
    TargetExists { from: PathBuf, to: PathBuf },
    /// The rename failed; the file keeps its old name.
    Failed {
        from: PathBuf,
        to: PathBuf,
        error: String,
    },
}

/// Everything one run did, unit by unit.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Report {
    pub units: Vec<UnitReport>,
    pub logs: Vec<LogOutcome>,
}

/// The one-word summary `--json` leads with, for TcrBar to branch on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// At least one thing was renamed, and nothing failed.
    Moved,
    /// Nothing needed moving, or both names exist and nothing may move.
    NothingToMove,
    /// At least one rename failed. Whatever failed is still at its old name.
    Failed,
    /// A tcr server is running, so nothing was looked at or moved.
    Refused,
}

impl Report {
    /// The summary: a failure outranks a move, a move outranks nothing.
    pub fn outcome(&self) -> Outcome {
        let unit_failed = self
            .units
            .iter()
            .any(|u| matches!(u.outcome, UnitOutcome::Failed { .. }));
        let log_failed = self
            .logs
            .iter()
            .any(|l| matches!(l, LogOutcome::Failed { .. }));
        let moved = self
            .units
            .iter()
            .any(|u| matches!(u.outcome, UnitOutcome::Moved { .. }))
            || self
                .logs
                .iter()
                .any(|l| matches!(l, LogOutcome::Renamed { .. }));
        if unit_failed || log_failed {
            Outcome::Failed
        } else if moved {
            Outcome::Moved
        } else {
            Outcome::NothingToMove
        }
    }

    /// The lines a person reads, each tagged with how loud it is. One per
    /// thing moved, with both paths; one per warning and failure, with both
    /// paths; and a closing line when nothing moved at all.
    pub fn lines(&self) -> Vec<(Level, String)> {
        let mut out = Vec::new();
        for report in &self.units {
            let label = report.unit.label();
            match &report.outcome {
                UnitOutcome::Moved { from, to } => out.push((
                    Level::Info,
                    format!("moved the {label} {} -> {}", from.display(), to.display()),
                )),
                UnitOutcome::BothExist { old, new } => out.push((
                    Level::Warn,
                    format!(
                        "both {} and {} exist; moved neither, and {} is the one in use",
                        old.display(),
                        new.display(),
                        new.display()
                    ),
                )),
                UnitOutcome::Failed { from, to, error } => out.push((
                    Level::Error,
                    format!(
                        "could not move the {label} {} -> {}: {error}; it stays at {} and is still used",
                        from.display(),
                        to.display(),
                        from.display()
                    ),
                )),
                UnitOutcome::AlreadyNew { .. } | UnitOutcome::Neither { .. } => {}
            }
        }
        for log in &self.logs {
            match log {
                LogOutcome::Renamed { from, to } => out.push((
                    Level::Info,
                    format!("renamed the log {} -> {}", from.display(), to.display()),
                )),
                LogOutcome::TargetExists { from, to } => out.push((
                    Level::Warn,
                    format!(
                        "left the log {} alone: {} already exists",
                        from.display(),
                        to.display()
                    ),
                )),
                LogOutcome::Failed { from, to, error } => out.push((
                    Level::Error,
                    format!(
                        "could not rename the log {} -> {}: {error}",
                        from.display(),
                        to.display()
                    ),
                )),
            }
        }
        if self.outcome() == Outcome::NothingToMove {
            out.push((
                Level::Info,
                "nothing to move: the config and cache already use the tcr names, or have none"
                    .to_string(),
            ));
        }
        out
    }
}

/// How loud a report line is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

/// A running server, which is why nothing moved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Refusal {
    pub port: u16,
    pub pid: u32,
    /// The one plain line the CLI prints.
    pub message: String,
}

impl Refusal {
    fn new(port: u16, incumbent: Incumbent) -> Self {
        let pid = incumbent.pid;
        // The advice must be survivable: an embedded proxy's pid is the host
        // application's, and a SIGTERM skips its shutdown (`singleton`).
        let stop = match incumbent.kind {
            ProxyKind::TcrEmbedded => "quit the app hosting it",
            ProxyKind::Tcr | ProxyKind::LegacyJs => "stop it",
        };
        Self {
            port,
            pid,
            message: format!(
                "a tcr server is running on port {port} (pid {pid}), and it would keep writing \
                 to the old files; {stop}, then run `tcr migrate`"
            ),
        }
    }
}

/// A whole run: refused, or the report of what it did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Run {
    Refused(Refusal),
    Done(Report),
}

impl Run {
    /// The summary word.
    pub fn outcome(&self) -> Outcome {
        match self {
            Self::Refused(_) => Outcome::Refused,
            Self::Done(report) => report.outcome(),
        }
    }

    /// The `--json` document: always the same four keys, so a reader never
    /// has to guess which ones a given outcome carries.
    pub fn to_json(&self) -> serde_json::Value {
        let empty = Report::default();
        let (refusal, report) = match self {
            Self::Refused(refusal) => (Some(refusal), &empty),
            Self::Done(report) => (None, report),
        };
        serde_json::json!({
            "outcome": self.outcome(),
            "refusal": refusal,
            "units": report.units,
            "logs": report.logs,
        })
    }

    /// The exit code: 0 when the run finished with nothing failed, 1 when a
    /// rename failed, 3 when it refused (2 is clap's, for a usage error).
    pub fn exit_code(&self) -> i32 {
        match self.outcome() {
            Outcome::Moved | Outcome::NothingToMove => 0,
            Outcome::Failed => 1,
            Outcome::Refused => 3,
        }
    }
}

/// The port a server using this install's config would bind: the config's
/// `proxy.port`, or the default when the config is missing or unreadable.
/// The same reading `tcr login` makes before it refuses.
fn configured_port(roots: &Roots) -> u16 {
    let path = paths::config_file_in(roots);
    match crate::config::load(&path) {
        Ok(config) => config.proxy.port,
        Err(err) => {
            let fallback = crate::config::ProxyConfig::default().port;
            if path.exists() {
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    port = fallback,
                    "could not read the config for its port; checking the default port for a running server"
                );
            }
            fallback
        }
    }
}

/// `tcr migrate`: refuse when `detect` finds a server on the configured port,
/// else move both units. `detect` is [`singleton::live_proxy_server`] in the
/// binary; a test passes its own.
pub fn run_in(roots: &Roots, detect: impl Fn(u16) -> Option<Incumbent>) -> Run {
    let port = configured_port(roots);
    if let Some(incumbent) = detect(port) {
        let refusal = Refusal::new(port, incumbent);
        tracing::warn!(port, pid = refusal.pid, "{}", refusal.message);
        return Run::Refused(refusal);
    }
    Run::Done(migrate_in(roots, |from, to| std::fs::rename(from, to)))
}

/// [`run_in`] for this process's own `HOME` and the real server detection.
pub fn run() -> Run {
    run_in(&Roots::from_env(), singleton::live_proxy_server)
}

/// Move both units under `roots`, with `rename` as the one filesystem write.
/// A test passes a failing `rename` to prove a failure leaves the old unit in
/// place. Does not check for a running server: [`run_in`] does that first.
pub fn migrate_in(roots: &Roots, rename: impl Fn(&Path, &Path) -> io::Result<()>) -> Report {
    // Each resolver returns one of its two names; the parent is the directory
    // both names share.
    let config_dir = paths::config_file_in(roots)
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let cache_base = paths::cache_dir_in(roots)
        .root()
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);

    let config = move_unit(
        Unit::Config,
        &config_dir.join(legacy_names::CONFIG_FILE),
        &config_dir.join(paths::CONFIG_FILE),
        // The same tests `paths` uses to pick a name, so a move never
        // disagrees with what the next read resolves.
        Path::exists,
        Path::exists,
        &rename,
    );
    let new_cache = cache_base.join(paths::CACHE_DIR);
    let cache = move_unit(
        Unit::Cache,
        &cache_base.join(legacy_names::CACHE_DIR),
        &new_cache,
        Path::is_dir,
        Path::is_dir,
        &rename,
    );
    let logs = if new_cache.is_dir() {
        rename_old_logs(&new_cache.join("logs"), &rename)
    } else {
        Vec::new()
    };
    Report {
        units: vec![config, cache],
        logs,
    }
}

/// One unit, by the rule in the module doc. `old_present` and `new_present`
/// are the existence tests [`crate::paths`] applies to this unit.
fn move_unit(
    unit: Unit,
    old: &Path,
    new: &Path,
    old_present: impl Fn(&Path) -> bool,
    new_present: impl Fn(&Path) -> bool,
    rename: impl Fn(&Path, &Path) -> io::Result<()>,
) -> UnitReport {
    let outcome = match (old_present(old), new_present(new)) {
        (true, false) => match rename(old, new) {
            Ok(()) => {
                tracing::info!(
                    from = %old.display(),
                    to = %new.display(),
                    "moved the {}",
                    unit.label()
                );
                UnitOutcome::Moved {
                    from: old.to_path_buf(),
                    to: new.to_path_buf(),
                }
            }
            Err(err) => {
                tracing::error!(
                    from = %old.display(),
                    to = %new.display(),
                    error = %err,
                    "could not move the {}; it stays at the old name and is still used",
                    unit.label()
                );
                UnitOutcome::Failed {
                    from: old.to_path_buf(),
                    to: new.to_path_buf(),
                    error: err.to_string(),
                }
            }
        },
        (true, true) => {
            tracing::warn!(
                old = %old.display(),
                new = %new.display(),
                "both names exist for the {}; moved neither, the new one is used",
                unit.label()
            );
            UnitOutcome::BothExist {
                old: old.to_path_buf(),
                new: new.to_path_buf(),
            }
        }
        (false, true) => UnitOutcome::AlreadyNew {
            path: new.to_path_buf(),
        },
        (false, false) => UnitOutcome::Neither {
            new: new.to_path_buf(),
        },
    };
    UnitReport { unit, outcome }
}

/// Rename every `<old prefix><suffix>` file in `log_dir` to
/// `<new prefix><suffix>`. A missing directory is nothing to do. A target that
/// already exists is left alone, never written over.
fn rename_old_logs(
    log_dir: &Path,
    rename: impl Fn(&Path, &Path) -> io::Result<()>,
) -> Vec<LogOutcome> {
    let entries = match std::fs::read_dir(log_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Vec::new(),
        Err(err) => {
            tracing::error!(
                dir = %log_dir.display(),
                error = %err,
                "could not list the log directory; old-prefix logs, if any, keep their names"
            );
            return vec![LogOutcome::Failed {
                from: log_dir.to_path_buf(),
                to: log_dir.to_path_buf(),
                error: format!("listing the directory: {err}"),
            }];
        }
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| match entry {
            Ok(entry) => Some(entry.file_name().to_string_lossy().into_owned()),
            Err(err) => {
                tracing::warn!(dir = %log_dir.display(), error = %err, "skipped an unreadable log directory entry");
                None
            }
        })
        .filter(|name| name.starts_with(legacy_names::LOG_PREFIX))
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let suffix = &name[legacy_names::LOG_PREFIX.len()..];
            let from = log_dir.join(&name);
            let to = log_dir.join(format!("{}{suffix}", paths::LOG_PREFIX));
            if to.exists() {
                tracing::warn!(from = %from.display(), to = %to.display(), "left an old-prefix log alone: the new name is taken");
                return LogOutcome::TargetExists { from, to };
            }
            match rename(&from, &to) {
                Ok(()) => {
                    tracing::info!(from = %from.display(), to = %to.display(), "renamed a log");
                    LogOutcome::Renamed { from, to }
                }
                Err(err) => {
                    tracing::error!(from = %from.display(), to = %to.display(), error = %err, "could not rename a log");
                    LogOutcome::Failed {
                        from,
                        to,
                        error: err.to_string(),
                    }
                }
            }
        })
        .collect()
}

/// Print a run the way `tcr migrate` does: the JSON document on stdout under
/// `json`, else the lines; warnings and errors always on stderr.
pub fn print(run: &Run, json: bool) {
    if json {
        println!("{}", run.to_json());
        return;
    }
    match run {
        Run::Refused(refusal) => eprintln!("tcr: {}", refusal.message),
        Run::Done(report) => {
            for (level, line) in report.lines() {
                match level {
                    Level::Info => println!("tcr: {line}"),
                    Level::Warn => eprintln!("tcr: warning: {line}"),
                    Level::Error => eprintln!("tcr: error: {line}"),
                }
            }
        }
    }
}

/// What `tcr update` does after a successful install or rebuild: move the
/// files when no server is running, else say when they will move. Never stops
/// the proxy, and never fails the update: the files still work where they are.
pub fn after_update() {
    match run() {
        Run::Refused(refusal) => println!(
            "tcr: the config and cache keep their old names for now: a tcr server is running on \
             port {} (pid {}). They move at the next update, or now with `tcr migrate` once the \
             proxy is stopped.",
            refusal.port, refusal.pid
        ),
        done => print(&done, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch {
        home: tempfile::TempDir,
        xdg: tempfile::TempDir,
    }

    impl Scratch {
        fn new() -> Self {
            Self {
                home: tempfile::tempdir().expect("temp HOME"),
                xdg: tempfile::tempdir().expect("temp XDG_CACHE_HOME"),
            }
        }

        fn roots(&self) -> Roots {
            Roots {
                home: Some(self.home.path().to_path_buf()),
                xdg_cache_home: Some(self.xdg.path().to_path_buf()),
            }
        }

        /// `name` in the config directory, which `paths` alone builds.
        fn config(&self, name: &str) -> PathBuf {
            paths::config_file_in(&self.roots())
                .parent()
                .expect("the config file has a directory")
                .join(name)
        }

        fn write_config(&self, name: &str, body: &str) {
            let path = self.config(name);
            std::fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir .config");
            std::fs::write(&path, body).expect("write config");
        }

        fn cache(&self, name: &str) -> PathBuf {
            self.xdg.path().join(name)
        }

        /// A cache directory holding a ledger file and the given log names.
        fn make_cache(&self, name: &str, logs: &[&str]) {
            let root = self.cache(name);
            std::fs::create_dir_all(root.join("logs")).expect("mkdir cache/logs");
            std::fs::write(root.join("session-affinity.json"), "{}").expect("write pins");
            for log in logs {
                std::fs::write(root.join("logs").join(log), "a log line\n").expect("write log");
            }
        }
    }

    fn real_rename(from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    fn outcome_of(report: &Report, unit: Unit) -> &UnitOutcome {
        &report
            .units
            .iter()
            .find(|u| u.unit == unit)
            .expect("every run reports both units")
            .outcome
    }

    fn old_log(suffix: &str) -> String {
        format!("{}{suffix}", legacy_names::LOG_PREFIX)
    }

    fn new_log(suffix: &str) -> String {
        format!("{}{suffix}", paths::LOG_PREFIX)
    }

    #[test]
    fn only_old_moves_both_units_and_the_next_read_finds_the_new_names() {
        let s = Scratch::new();
        s.write_config(legacy_names::CONFIG_FILE, r#"{"accounts":[]}"#);
        s.make_cache(legacy_names::CACHE_DIR, &[]);

        let report = migrate_in(&s.roots(), real_rename);

        assert_eq!(report.outcome(), Outcome::Moved, "{report:?}");
        assert!(matches!(
            outcome_of(&report, Unit::Config),
            UnitOutcome::Moved { .. }
        ));
        assert!(matches!(
            outcome_of(&report, Unit::Cache),
            UnitOutcome::Moved { .. }
        ));
        assert!(s.config(paths::CONFIG_FILE).is_file());
        assert!(!s.config(legacy_names::CONFIG_FILE).exists());
        assert!(
            s.cache(paths::CACHE_DIR)
                .join("session-affinity.json")
                .is_file()
        );
        assert!(!s.cache(legacy_names::CACHE_DIR).exists());
        assert_eq!(
            paths::config_file_in(&s.roots()),
            s.config(paths::CONFIG_FILE)
        );
        assert_eq!(
            paths::cache_dir_in(&s.roots()).generation(),
            paths::Generation::Current
        );
        // One line per thing moved, each naming both paths.
        let lines = report.lines();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(
            lines[0]
                .1
                .contains(&s.config(legacy_names::CONFIG_FILE).display().to_string())
        );
        assert!(
            lines[0]
                .1
                .contains(&s.config(paths::CONFIG_FILE).display().to_string())
        );
    }

    #[test]
    fn both_exist_moves_nothing_and_warns_naming_both() {
        let s = Scratch::new();
        s.write_config(legacy_names::CONFIG_FILE, "old");
        s.write_config(paths::CONFIG_FILE, "new");
        s.make_cache(legacy_names::CACHE_DIR, &[]);
        s.make_cache(paths::CACHE_DIR, &[]);

        let report = migrate_in(&s.roots(), real_rename);

        assert_eq!(report.outcome(), Outcome::NothingToMove);
        assert!(matches!(
            outcome_of(&report, Unit::Config),
            UnitOutcome::BothExist { .. }
        ));
        assert!(matches!(
            outcome_of(&report, Unit::Cache),
            UnitOutcome::BothExist { .. }
        ));
        assert_eq!(
            std::fs::read_to_string(s.config(legacy_names::CONFIG_FILE)).expect("old kept"),
            "old"
        );
        assert_eq!(
            std::fs::read_to_string(s.config(paths::CONFIG_FILE)).expect("new kept"),
            "new"
        );
        assert!(s.cache(legacy_names::CACHE_DIR).is_dir());
        let warnings: Vec<_> = report
            .lines()
            .into_iter()
            .filter(|(level, _)| *level == Level::Warn)
            .map(|(_, line)| line)
            .collect();
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        let old = s.config(legacy_names::CONFIG_FILE).display().to_string();
        let new = s.config(paths::CONFIG_FILE).display().to_string();
        assert!(
            warnings[0].contains(&old) && warnings[0].contains(&new),
            "{}",
            warnings[0]
        );
    }

    #[test]
    fn only_new_and_neither_do_nothing() {
        let only_new = Scratch::new();
        only_new.write_config(paths::CONFIG_FILE, "{}");
        only_new.make_cache(paths::CACHE_DIR, &[]);
        let report = migrate_in(&only_new.roots(), real_rename);
        assert_eq!(report.outcome(), Outcome::NothingToMove);
        assert!(matches!(
            outcome_of(&report, Unit::Config),
            UnitOutcome::AlreadyNew { .. }
        ));
        assert!(matches!(
            outcome_of(&report, Unit::Cache),
            UnitOutcome::AlreadyNew { .. }
        ));

        let neither = Scratch::new();
        let report = migrate_in(&neither.roots(), |_: &Path, _: &Path| {
            panic!("nothing exists, so nothing may be renamed")
        });
        assert_eq!(report.outcome(), Outcome::NothingToMove);
        assert!(matches!(
            outcome_of(&report, Unit::Config),
            UnitOutcome::Neither { .. }
        ));
        assert!(matches!(
            outcome_of(&report, Unit::Cache),
            UnitOutcome::Neither { .. }
        ));
        assert!(!neither.config(paths::CONFIG_FILE).exists());
        assert!(!neither.cache(paths::CACHE_DIR).exists());
    }

    #[test]
    fn a_failed_rename_leaves_the_old_unit_in_place_and_still_used() {
        let s = Scratch::new();
        s.write_config(legacy_names::CONFIG_FILE, "{}");
        s.make_cache(legacy_names::CACHE_DIR, &[&old_log(".2026-10-01")]);

        let report = migrate_in(&s.roots(), |_: &Path, _: &Path| {
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        });

        assert_eq!(report.outcome(), Outcome::Failed);
        assert!(matches!(
            outcome_of(&report, Unit::Config),
            UnitOutcome::Failed { .. }
        ));
        assert!(matches!(
            outcome_of(&report, Unit::Cache),
            UnitOutcome::Failed { .. }
        ));
        assert!(s.config(legacy_names::CONFIG_FILE).is_file());
        assert!(!s.config(paths::CONFIG_FILE).exists());
        assert_eq!(
            paths::config_file_in(&s.roots()),
            s.config(legacy_names::CONFIG_FILE),
            "the read fallback still finds the old config"
        );
        assert_eq!(
            paths::cache_dir_in(&s.roots()).root(),
            s.cache(legacy_names::CACHE_DIR),
            "the read fallback still finds the old cache"
        );
        assert!(
            report.logs.is_empty(),
            "logs in a directory that did not move are not renamed"
        );
        let errors: Vec<_> = report
            .lines()
            .into_iter()
            .filter(|(level, _)| *level == Level::Error)
            .collect();
        assert_eq!(errors.len(), 2, "{errors:?}");
        let run = Run::Done(report);
        assert_eq!(run.exit_code(), 1);
    }

    #[test]
    fn old_prefix_logs_are_renamed_inside_the_moved_directory() {
        let s = Scratch::new();
        s.make_cache(
            legacy_names::CACHE_DIR,
            &[
                &old_log(""),
                &old_log(".2026-10-15"),
                &old_log(".2026-10-16"),
                "unrelated.txt",
            ],
        );

        let report = migrate_in(&s.roots(), real_rename);

        let logs = s.cache(paths::CACHE_DIR).join("logs");
        let mut names: Vec<String> = std::fs::read_dir(&logs)
            .expect("logs moved with the directory")
            .map(|e| {
                e.expect("an entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        let mut want = vec![
            new_log(""),
            new_log(".2026-10-15"),
            new_log(".2026-10-16"),
            "unrelated.txt".to_string(),
        ];
        want.sort();
        assert_eq!(names, want);
        assert_eq!(report.logs.len(), 3, "{:?}", report.logs);
        assert!(
            report
                .logs
                .iter()
                .all(|l| matches!(l, LogOutcome::Renamed { .. }))
        );
    }

    #[test]
    fn a_log_whose_new_name_is_taken_is_left_alone() {
        let s = Scratch::new();
        s.make_cache(
            paths::CACHE_DIR,
            &[&old_log(".2026-10-16"), &new_log(".2026-10-16")],
        );
        let report = migrate_in(&s.roots(), real_rename);
        assert!(matches!(
            report.logs.as_slice(),
            [LogOutcome::TargetExists { .. }]
        ));
        let logs = s.cache(paths::CACHE_DIR).join("logs");
        assert!(logs.join(old_log(".2026-10-16")).is_file());
        assert!(logs.join(new_log(".2026-10-16")).is_file());
    }

    #[test]
    fn a_second_run_moves_nothing_and_says_so() {
        let s = Scratch::new();
        s.write_config(legacy_names::CONFIG_FILE, "{}");
        s.make_cache(legacy_names::CACHE_DIR, &[&old_log(".2026-10-16")]);
        assert_eq!(
            migrate_in(&s.roots(), real_rename).outcome(),
            Outcome::Moved
        );

        let second = migrate_in(&s.roots(), |_: &Path, _: &Path| {
            panic!("the second run must rename nothing")
        });
        assert_eq!(second.outcome(), Outcome::NothingToMove);
        let lines = second.lines();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].1.starts_with("nothing to move"), "{lines:?}");
    }

    #[test]
    fn a_running_server_refuses_and_moves_nothing() {
        let s = Scratch::new();
        s.write_config(
            legacy_names::CONFIG_FILE,
            r#"{"accounts":[],"proxy":{"port":45678}}"#,
        );
        s.make_cache(legacy_names::CACHE_DIR, &[]);
        let asked = std::cell::Cell::new(None);
        let run = run_in(&s.roots(), |port| {
            asked.set(Some(port));
            Some(Incumbent {
                pid: 4242,
                kind: ProxyKind::Tcr,
            })
        });
        assert_eq!(
            asked.get(),
            Some(45678),
            "detection asks about the configured port"
        );
        assert_eq!(run.outcome(), Outcome::Refused);
        assert_eq!(run.exit_code(), 3);
        assert!(s.config(legacy_names::CONFIG_FILE).is_file());
        assert!(!s.config(paths::CONFIG_FILE).exists());
        assert!(s.cache(legacy_names::CACHE_DIR).is_dir());
        let json = run.to_json();
        assert_eq!(json["outcome"], "refused");
        assert_eq!(json["refusal"]["pid"], 4242);
        assert_eq!(json["units"], serde_json::json!([]));
    }

    /// The `--json` shape TcrBar parses, pinned per outcome.
    #[test]
    fn the_json_document_has_the_same_keys_for_every_outcome() {
        let s = Scratch::new();
        s.write_config(legacy_names::CONFIG_FILE, "{}");
        let run = run_in(&s.roots(), |_| None);
        let json = run.to_json();
        assert_eq!(json["outcome"], "moved");
        assert_eq!(json["refusal"], serde_json::Value::Null);
        assert_eq!(json["units"][0]["unit"], "config");
        assert_eq!(json["units"][0]["result"], "moved");
        assert_eq!(
            json["units"][0]["to"],
            s.config(paths::CONFIG_FILE).display().to_string()
        );
        assert_eq!(json["units"][1]["unit"], "cache");
        assert_eq!(json["units"][1]["result"], "neither");
        assert_eq!(json["logs"], serde_json::json!([]));
    }
}
