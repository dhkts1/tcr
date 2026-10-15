//! Where `tcr` keeps its files: the only code that builds an on-disk path for
//! the config file, the cache directory and the logs.
//!
//! A fresh install gets `~/.config/tcr.json` and `<cache base>/tcr/`. An install
//! from before the rename keeps the `teamclaude` names it already has
//! ([`crate::legacy_names`]), read and written where they are. Nothing is
//! moved, copied or symlinked; that module's doc says why.
//!
//! **The rule, decided per unit:** use the new path if it exists, else the old
//! path if it exists, else the new path. The units are the config file and the
//! cache directory. The cache directory is one unit: the usage ledger, the
//! session-affinity pins, the session-wire file, peer state and the logs all
//! follow the one resolved directory, so an install never splits its cache
//! across two folders. The log file prefix follows the directory too, because
//! `tracing_appender` prunes by prefix.
//!
//! The config lives in `~/.config` rather than the platform config dir
//! (`~/Library/Application Support` on macOS) because the JS proxy hard-codes
//! `~/.config`, and this binary was a drop-in for it.

use std::path::{Path, PathBuf};

use crate::legacy_names;

/// The config file a fresh install gets, in `~/.config`.
pub const CONFIG_FILE: &str = "tcr.json";

/// The cache directory a fresh install gets, under the cache base.
pub const CACHE_DIR: &str = "tcr";

/// The rotating log file prefix inside the new cache directory.
pub const LOG_PREFIX: &str = "tcr.log";

/// The environment the paths are resolved from. Read once by
/// [`Roots::from_env`]; a test builds one over temp dirs instead of changing
/// this process's environment, which every other test shares.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Roots {
    /// `$HOME`, as set. Not filtered for empty, matching the fallback chain
    /// every caller used before this module existed.
    pub home: Option<PathBuf>,
    /// `$XDG_CACHE_HOME` when set and non-empty.
    pub xdg_cache_home: Option<PathBuf>,
}

impl Roots {
    /// This process's own `HOME` and `XDG_CACHE_HOME`.
    pub fn from_env() -> Self {
        Self {
            home: std::env::var_os("HOME").map(PathBuf::from),
            xdg_cache_home: std::env::var_os("XDG_CACHE_HOME")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
        }
    }

    /// `$HOME/.config`, or `./.config` with no `HOME`.
    fn config_base(&self) -> PathBuf {
        self.home
            .clone()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".config")
    }

    /// `$XDG_CACHE_HOME`, else `$HOME/.cache`, else `.`.
    fn cache_base(&self) -> PathBuf {
        self.xdg_cache_home
            .clone()
            .or_else(|| self.home.as_ref().map(|home| home.join(".cache")))
            .unwrap_or_else(|| PathBuf::from("."))
    }
}

/// Which of the two names a unit resolved to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Generation {
    /// The `tcr` name: a fresh install, or one that already has the new path.
    Current,
    /// The `teamclaude` name, kept because only it exists on disk.
    Legacy,
}

/// The rule: new if it exists, else old if it exists, else new.
fn pick(new_exists: bool, old_exists: bool) -> Generation {
    if !new_exists && old_exists {
        Generation::Legacy
    } else {
        Generation::Current
    }
}

/// The config file under `roots`: `~/.config/tcr.json`, or
/// `~/.config/teamclaude.json` when only that one exists.
pub fn config_file_in(roots: &Roots) -> PathBuf {
    let base = roots.config_base();
    let new = base.join(CONFIG_FILE);
    let old = base.join(legacy_names::CONFIG_FILE);
    match pick(new.exists(), old.exists()) {
        Generation::Current => new,
        Generation::Legacy => old,
    }
}

/// The default config file for this process. See [`config_file_in`].
pub fn config_file() -> PathBuf {
    config_file_in(&Roots::from_env())
}

/// The resolved cache directory and everything that lives in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheDir {
    root: PathBuf,
    generation: Generation,
}

impl CacheDir {
    /// The directory itself: `<cache base>/tcr` or `<cache base>/teamclaude`.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Which name the directory resolved to.
    pub fn generation(&self) -> Generation {
        self.generation
    }

    /// The usage ledger directory: `<root>/usage/`.
    pub fn usage_dir(&self) -> PathBuf {
        self.root.join("usage")
    }

    /// The session-affinity pin file: `<root>/session-affinity.json`.
    pub fn affinity_file(&self) -> PathBuf {
        self.root.join("session-affinity.json")
    }

    /// The session-wire file: `<root>/session-wire.json`.
    pub fn session_wire_file(&self) -> PathBuf {
        self.root.join("session-wire.json")
    }

    /// Peer runtime state: `<root>/peer-state.json`.
    pub fn peer_state_file(&self) -> PathBuf {
        self.root.join("peer-state.json")
    }

    /// The log directory: `<root>/logs/`.
    pub fn log_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    /// The log file prefix for this directory: `tcr.log` in the new one,
    /// `teamclaude-rs.log` in the old one, so the old logs keep being pruned.
    pub fn log_prefix(&self) -> &'static str {
        match self.generation {
            Generation::Current => LOG_PREFIX,
            Generation::Legacy => legacy_names::LOG_PREFIX,
        }
    }
}

/// The cache directory under `roots`, decided once for the whole directory.
pub fn cache_dir_in(roots: &Roots) -> CacheDir {
    let base = roots.cache_base();
    let new = base.join(CACHE_DIR);
    let old = base.join(legacy_names::CACHE_DIR);
    let generation = pick(new.is_dir(), old.is_dir());
    let root = match generation {
        Generation::Current => new,
        Generation::Legacy => old,
    };
    CacheDir { root, generation }
}

/// The cache directory for this process. See [`cache_dir_in`].
pub fn cache_dir() -> CacheDir {
    cache_dir_in(&Roots::from_env())
}

/// What every `--config` flag's help says about the default, appended to the
/// flag's own description by the CLI. Written once here so 31 flags cannot
/// drift apart.
pub fn config_flag_default_help() -> String {
    format!(
        "Default: ~/.config/{CONFIG_FILE}, or ~/.config/{} when only that one exists",
        legacy_names::CONFIG_FILE
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temp `HOME` and a temp `XDG_CACHE_HOME`, empty.
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

        fn config(&self, name: &str) -> PathBuf {
            self.home.path().join(".config").join(name)
        }

        fn write_config(&self, name: &str) {
            let path = self.config(name);
            std::fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir .config");
            std::fs::write(&path, "{}").expect("write config");
        }

        fn cache(&self, name: &str) -> PathBuf {
            self.xdg.path().join(name)
        }

        fn make_cache(&self, name: &str) {
            std::fs::create_dir_all(self.cache(name)).expect("mkdir cache");
        }
    }

    #[test]
    fn config_old_only_keeps_the_old_file() {
        let s = Scratch::new();
        s.write_config(legacy_names::CONFIG_FILE);
        assert_eq!(
            config_file_in(&s.roots()),
            s.config(legacy_names::CONFIG_FILE)
        );
        assert!(
            !s.config(CONFIG_FILE).exists(),
            "resolving must not create the new file"
        );
    }

    #[test]
    fn config_new_only_uses_the_new_file() {
        let s = Scratch::new();
        s.write_config(CONFIG_FILE);
        assert_eq!(config_file_in(&s.roots()), s.config(CONFIG_FILE));
    }

    #[test]
    fn config_both_the_new_one_wins() {
        let s = Scratch::new();
        s.write_config(CONFIG_FILE);
        s.write_config(legacy_names::CONFIG_FILE);
        assert_eq!(config_file_in(&s.roots()), s.config(CONFIG_FILE));
    }

    #[test]
    fn config_neither_is_the_new_file() {
        let s = Scratch::new();
        assert_eq!(config_file_in(&s.roots()), s.config(CONFIG_FILE));
        assert!(
            !s.config(CONFIG_FILE).exists() && !s.config(legacy_names::CONFIG_FILE).exists(),
            "resolving creates nothing; the first write does"
        );
    }

    /// Neither exists, so the new file is the one that gets created, and from
    /// then on it is the one resolved.
    #[test]
    fn config_neither_then_created_resolves_to_the_created_file() {
        let s = Scratch::new();
        let chosen = config_file_in(&s.roots());
        crate::config::load_or_init(&chosen).expect("first run creates the config");
        assert!(chosen.is_file(), "the new file was created");
        assert_eq!(config_file_in(&s.roots()), s.config(CONFIG_FILE));
        assert!(!s.config(legacy_names::CONFIG_FILE).exists());
    }

    #[test]
    fn cache_old_only_keeps_the_old_directory_and_its_log_prefix() {
        let s = Scratch::new();
        s.make_cache(legacy_names::CACHE_DIR);
        let cache = cache_dir_in(&s.roots());
        let old = s.cache(legacy_names::CACHE_DIR);
        assert_eq!(cache.generation(), Generation::Legacy);
        assert_eq!(cache.root(), old);
        // Every file in the unit follows the one directory.
        assert_eq!(cache.usage_dir(), old.join("usage"));
        assert_eq!(cache.affinity_file(), old.join("session-affinity.json"));
        assert_eq!(cache.session_wire_file(), old.join("session-wire.json"));
        assert_eq!(cache.peer_state_file(), old.join("peer-state.json"));
        assert_eq!(cache.log_dir(), old.join("logs"));
        assert_eq!(cache.log_prefix(), legacy_names::LOG_PREFIX);
        assert!(!s.cache(CACHE_DIR).exists(), "resolving creates nothing");
    }

    #[test]
    fn cache_new_only_uses_the_new_directory() {
        let s = Scratch::new();
        s.make_cache(CACHE_DIR);
        let cache = cache_dir_in(&s.roots());
        assert_eq!(cache.generation(), Generation::Current);
        assert_eq!(cache.root(), s.cache(CACHE_DIR));
        assert_eq!(cache.log_prefix(), LOG_PREFIX);
    }

    #[test]
    fn cache_both_the_new_one_wins() {
        let s = Scratch::new();
        s.make_cache(CACHE_DIR);
        s.make_cache(legacy_names::CACHE_DIR);
        let cache = cache_dir_in(&s.roots());
        assert_eq!(cache.root(), s.cache(CACHE_DIR));
        assert_eq!(cache.log_dir(), s.cache(CACHE_DIR).join("logs"));
        assert_eq!(cache.log_prefix(), LOG_PREFIX);
    }

    #[test]
    fn cache_neither_is_the_new_directory() {
        let s = Scratch::new();
        let cache = cache_dir_in(&s.roots());
        assert_eq!(cache.generation(), Generation::Current);
        assert_eq!(cache.root(), s.cache(CACHE_DIR));
        assert_eq!(
            cache.affinity_file(),
            s.cache(CACHE_DIR).join("session-affinity.json")
        );
        assert!(!s.cache(CACHE_DIR).exists() && !s.cache(legacy_names::CACHE_DIR).exists());
    }

    /// The cache base keeps the old fallback chain: a non-empty
    /// `XDG_CACHE_HOME`, else `$HOME/.cache`, else the working directory.
    #[test]
    fn cache_base_falls_back_to_home_then_dot() {
        let s = Scratch::new();
        let no_xdg = Roots {
            home: Some(s.home.path().to_path_buf()),
            xdg_cache_home: None,
        };
        assert_eq!(
            cache_dir_in(&no_xdg).root(),
            s.home.path().join(".cache").join(CACHE_DIR)
        );
        std::fs::create_dir_all(s.home.path().join(".cache").join(legacy_names::CACHE_DIR))
            .expect("mkdir old cache under HOME");
        assert_eq!(
            cache_dir_in(&no_xdg).root(),
            s.home.path().join(".cache").join(legacy_names::CACHE_DIR)
        );
        let nothing = Roots::default();
        assert_eq!(cache_dir_in(&nothing).root().parent(), Some(Path::new(".")));
        assert_eq!(
            config_file_in(&nothing).parent(),
            Some(Path::new("./.config"))
        );
    }

    /// A FILE named like the cache directory is not a cache directory.
    #[test]
    fn a_file_where_the_new_cache_dir_would_be_does_not_win() {
        let s = Scratch::new();
        std::fs::write(s.cache(CACHE_DIR), "").expect("write a stray file");
        s.make_cache(legacy_names::CACHE_DIR);
        assert_eq!(
            cache_dir_in(&s.roots()).root(),
            s.cache(legacy_names::CACHE_DIR)
        );
    }

    #[test]
    fn the_flag_help_names_both_files() {
        let help = config_flag_default_help();
        assert!(help.contains(&format!("~/.config/{CONFIG_FILE}")), "{help}");
        assert!(help.contains(legacy_names::CONFIG_FILE), "{help}");
    }
}
