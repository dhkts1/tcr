//! Where `tcr` keeps its files: the only code that builds an on-disk path for
//! the config file, the cache directory and the logs.
//!
//! The config file is `~/.config/tcr.json` and the cache directory is
//! `<cache base>/tcr/`. Resolving a path never creates, moves, copies or
//! symlinks anything; the first write does.
//!
//! The cache directory is one unit: the usage ledger, the session-affinity
//! pins, the session-wire file, peer state and the logs all live under the one
//! directory.

use std::path::{Path, PathBuf};

/// The config file, in `~/.config`.
pub const CONFIG_FILE: &str = "tcr.json";

/// The cache directory, under the cache base.
pub const CACHE_DIR: &str = "tcr";

/// The rotating log file prefix inside the cache directory.
pub const LOG_PREFIX: &str = "tcr.log";

/// Codex's auth document inside an explicitly selected `CODEX_HOME`.
pub const CODEX_AUTH_FILE: &str = "auth.json";
/// The stable advisory lock shared by tcr's imported-login writers; Codex does not take it.
const CODEX_AUTH_LOCK_FILE: &str = ".auth.json.tcr-lock";
/// A private temporary sibling used to publish a guarded imported-login update.
pub(crate) const CODEX_AUTH_TEMP_PREFIX: &str = ".auth.json.tcr-";
/// A throwaway `CODEX_HOME` beside the selected tcr config, removed when sign-in ends.
pub(crate) const CODEX_LOGIN_PREFIX: &str = "tcr-codex-login.";

/// Codex's state directory: an explicitly selected home wins, otherwise HOME/.codex.
/// Inputs are already read by the caller so importing a login never reads an ambient home here.
pub fn codex_home_in(codex_home: Option<PathBuf>, user_home: Option<PathBuf>) -> PathBuf {
    codex_home.unwrap_or_else(|| {
        user_home
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".codex")
    })
}

/// The auth document in the supplied Codex home; never resolves a default credential store.
pub fn codex_auth_file(codex_home: &Path) -> PathBuf {
    codex_home.join(CODEX_AUTH_FILE)
}

/// The writer lock in the supplied Codex home; never resolves an ambient credential store.
pub(crate) fn codex_auth_lock_file(codex_home: &Path) -> PathBuf {
    codex_home.join(CODEX_AUTH_LOCK_FILE)
}

/// The parent for a throwaway login beside the selected config, including relative configs.
pub(crate) fn codex_login_parent(config_file: &Path) -> &Path {
    config_file
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

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

/// The config file under `roots`: `~/.config/tcr.json`.
pub fn config_file_in(roots: &Roots) -> PathBuf {
    roots.config_base().join(CONFIG_FILE)
}

/// The default config file for this process. See [`config_file_in`].
pub fn config_file() -> PathBuf {
    config_file_in(&Roots::from_env())
}

/// The cache directory and everything that lives in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheDir {
    root: PathBuf,
}

impl CacheDir {
    /// The directory itself: `<cache base>/tcr`.
    pub fn root(&self) -> &Path {
        &self.root
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
}

/// The cache directory under `roots`.
pub fn cache_dir_in(roots: &Roots) -> CacheDir {
    CacheDir {
        root: roots.cache_base().join(CACHE_DIR),
    }
}

/// The cache directory for this process. See [`cache_dir_in`].
pub fn cache_dir() -> CacheDir {
    cache_dir_in(&Roots::from_env())
}

/// What every `--config` flag's help says about the default, appended to the
/// flag's own description by the CLI. Written once here so 31 flags cannot
/// drift apart.
pub fn config_flag_default_help() -> String {
    format!("Default: ~/.config/{CONFIG_FILE}")
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

        fn cache(&self, name: &str) -> PathBuf {
            self.xdg.path().join(name)
        }
    }

    #[test]
    fn config_is_tcr_json_and_resolving_creates_nothing() {
        let s = Scratch::new();
        assert_eq!(config_file_in(&s.roots()), s.config(CONFIG_FILE));
        assert!(
            !s.config(CONFIG_FILE).exists(),
            "resolving creates nothing; the first write does"
        );
    }

    /// The first run creates the resolved file, and from then on it is the one
    /// resolved.
    #[test]
    fn config_created_on_first_run_resolves_to_the_created_file() {
        let s = Scratch::new();
        let chosen = config_file_in(&s.roots());
        crate::config::load_or_init(&chosen).expect("first run creates the config");
        assert!(chosen.is_file(), "the file was created");
        assert_eq!(config_file_in(&s.roots()), s.config(CONFIG_FILE));
    }

    #[test]
    fn every_cache_file_follows_the_one_directory() {
        let s = Scratch::new();
        let cache = cache_dir_in(&s.roots());
        let root = s.cache(CACHE_DIR);
        assert_eq!(cache.root(), root);
        assert_eq!(cache.usage_dir(), root.join("usage"));
        assert_eq!(cache.affinity_file(), root.join("session-affinity.json"));
        assert_eq!(cache.session_wire_file(), root.join("session-wire.json"));
        assert_eq!(cache.peer_state_file(), root.join("peer-state.json"));
        assert_eq!(cache.log_dir(), root.join("logs"));
        assert!(!root.exists(), "resolving creates nothing");
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
        let nothing = Roots::default();
        assert_eq!(cache_dir_in(&nothing).root().parent(), Some(Path::new(".")));
        assert_eq!(
            config_file_in(&nothing).parent(),
            Some(Path::new("./.config"))
        );
    }

    #[test]
    fn the_flag_help_names_the_default_file() {
        let help = config_flag_default_help();
        assert_eq!(help, format!("Default: ~/.config/{CONFIG_FILE}"));
    }

    /// `source` with every comment replaced by spaces (newlines kept, so line
    /// numbers survive) and every string and char literal left as written.
    ///
    /// A lexer rather than "drop everything after `//`", because `//` occurs
    /// inside string literals (`"https://..."`) and a char literal can hold a
    /// quote (`'"'`) that would otherwise open a phantom string.
    fn strip_comments(source: &str) -> String {
        let chars: Vec<char> = source.chars().collect();
        let mut out = String::with_capacity(source.len());
        let blank = |c: char| if c == '\n' { '\n' } else { ' ' };
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            let next = chars.get(i + 1).copied();
            let prev_is_ident = i > 0 && (chars[i - 1].is_alphanumeric() || chars[i - 1] == '_');
            if c == '/' && next == Some('/') {
                while i < chars.len() && chars[i] != '\n' {
                    out.push(' ');
                    i += 1;
                }
            } else if c == '/' && next == Some('*') {
                let mut depth = 0usize;
                while i < chars.len() {
                    if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
                        depth += 1;
                        out.push_str("  ");
                        i += 2;
                    } else if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                        depth -= 1;
                        out.push_str("  ");
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        out.push(blank(chars[i]));
                        i += 1;
                    }
                }
            } else if c == 'r'
                && !(prev_is_ident && chars[i - 1] != 'b')
                && matches!(next, Some('"' | '#'))
            {
                // A raw string: r"..." or r#"..."#, possibly after a `b`.
                let mut j = i + 1;
                let mut hashes = 0;
                while chars.get(j) == Some(&'#') {
                    hashes += 1;
                    j += 1;
                }
                if chars.get(j) != Some(&'"') {
                    // `r#ident`, a raw identifier.
                    out.push(c);
                    i += 1;
                    continue;
                }
                j += 1;
                loop {
                    match chars.get(j) {
                        None => break,
                        Some('"') if (1..=hashes).all(|k| chars.get(j + k) == Some(&'#')) => {
                            j += 1 + hashes;
                            break;
                        }
                        Some(_) => j += 1,
                    }
                }
                out.extend(&chars[i..j]);
                i = j;
            } else if c == '"' {
                let mut j = i + 1;
                while j < chars.len() && chars[j] != '"' {
                    j += if chars[j] == '\\' { 2 } else { 1 };
                }
                let end = (j + 1).min(chars.len());
                out.extend(&chars[i..end]);
                i = end;
            } else if c == '\'' {
                // A char literal, or a lifetime / loop label.
                let end = if next == Some('\\') {
                    // Skip the backslash and the escaped char, so `'\''` ends
                    // at its last quote rather than the escaped one.
                    let mut j = i + 3;
                    while j < chars.len() && chars[j] != '\'' {
                        j += 1;
                    }
                    j + 1
                } else if chars.get(i + 2) == Some(&'\'') {
                    i + 3
                } else {
                    i + 1
                };
                let end = end.min(chars.len());
                out.extend(&chars[i..end]);
                i = end;
            } else {
                out.push(c);
                i += 1;
            }
        }
        out
    }

    fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
        let entries = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read {dir:?}: {e}"));
        for entry in entries {
            let path = entry.expect("a directory entry").path();
            if path.is_dir() {
                rust_files(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }

    /// Every `(line, code)` in `source` that joins `.cache` or `.config` onto a
    /// path (including `.config/fish`), comments removed. Spacing inside the
    /// call is ignored, so `.join( ".cache" )` is seen too.
    fn dot_dir_joins(source: &str) -> Vec<(usize, String)> {
        strip_comments(source)
            .lines()
            .enumerate()
            .filter(|(_, line)| {
                let squeezed: String = line.chars().filter(|c| !c.is_whitespace()).collect();
                squeezed.contains("join(\".cache") || squeezed.contains("join(\".config")
            })
            .map(|(n, line)| (n + 1, line.trim().to_string()))
            .collect()
    }

    /// The one site outside `src/paths.rs` that may join `.config`, named with
    /// its reason: the fish shell's own config directory, which is fish's path
    /// and not a `tcr` one.
    const FISH_DIR_JOIN: (&str, &str) = (
        "src/setup.rs",
        ".unwrap_or_else(|| self.home.join(\".config\"))",
    );

    /// Routing: `src/paths.rs` is the only code that builds the `~/.config`
    /// and `~/.cache` bases, so a second resolution of the cache directory
    /// (one that ignores `XDG_CACHE_HOME`, say) cannot split the cache across
    /// two folders. The walk covers `src/` and every crate under `crates/`; no
    /// crate builds such a path today, and this keeps it that way.
    #[test]
    fn dot_dir_paths_are_built_only_in_paths() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let paths_rs = root.join("src").join("paths.rs");
        let mut files = Vec::new();
        rust_files(&root.join("src"), &mut files);
        rust_files(&root.join("crates"), &mut files);
        assert!(
            files.contains(&paths_rs) && files.iter().any(|f| f.starts_with(root.join("crates"))),
            "the walk must reach src/paths.rs and the crates, found {} files",
            files.len()
        );
        let mut found = Vec::new();
        let mut allowed_seen = 0;
        for file in files.iter().filter(|f| **f != paths_rs) {
            let rel = file.strip_prefix(root).unwrap_or(file);
            let source =
                std::fs::read_to_string(file).unwrap_or_else(|e| panic!("read {file:?}: {e}"));
            for (line, code) in dot_dir_joins(&source) {
                if rel == Path::new(FISH_DIR_JOIN.0) && code == FISH_DIR_JOIN.1 {
                    allowed_seen += 1;
                } else {
                    found.push(format!("{}:{line}: {code}", rel.display()));
                }
            }
        }
        assert!(
            found.is_empty(),
            "`.cache` and `.config` are joined in src/paths.rs only; call its functions:\n{}",
            found.join("\n")
        );
        assert_eq!(
            allowed_seen, 1,
            "the allowed fish-directory join moved or vanished: update FISH_DIR_JOIN"
        );
    }

    /// The positive control for [`dot_dir_joins`]: it sees the shapes code can
    /// write, and leaves comments and unrelated joins alone.
    #[test]
    fn the_routing_guard_sees_joins_and_ignores_comments() {
        let code = [
            "let a = home.join(\".cache\").join(\"tcr\");",
            "let b = home.join( \".config\" );",
            "let c = base.join(\".config/fish\");",
            "let d = home\n    .join(\".cache\");",
        ]
        .join("\n");
        let lines: Vec<usize> = dot_dir_joins(&code).into_iter().map(|(n, _)| n).collect();
        assert_eq!(lines, vec![1, 2, 3, 5]);
        assert_eq!(
            dot_dir_joins("// home.join(\".cache\")\nlet e = home.join(\".claude\");\n"),
            Vec::new()
        );
        // A quote inside a char literal must not open a string that swallows
        // the comment marker after it.
        assert_eq!(
            dot_dir_joins("let q = '\"'; // home.join(\".cache\")\nlet r = 'a';\n"),
            Vec::new()
        );
        // `//` inside a string is not a comment.
        assert_eq!(
            dot_dir_joins("let u = \"https://x\"; let v = home.join(\".cache\");\n"),
            vec![(
                1,
                "let u = \"https://x\"; let v = home.join(\".cache\");".to_string()
            )]
        );
    }
}
