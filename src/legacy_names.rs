//! Every on-disk and process name `tcr` inherited from `teamclaude`, in one file.
//!
//! The project is being renamed from `teamclaude-rs` to `tcr`. A fresh install
//! gets the new names, which live in [`crate::paths`]. An install that already
//! has the old files keeps using them **where they are** until an update runs
//! `tcr migrate` ([`crate::migrate`]), which renames the config file and the
//! cache directory and nothing else. Nothing is ever copied or symlinked:
//!
//! - Users were told to `export NODE_EXTRA_CA_CERTS=<path to the CA>`, so the
//!   Node proxy's CA files below can never move without breaking every client
//!   that trusts them. `tcr migrate` leaves them alone.
//! - The config is written atomically (temp file plus rename), which replaces a
//!   symlink with a real file. A symlinked config would split in two on the
//!   first write, which is why the move is one `rename(2)`.
//!
//! Each constant says why it still exists. **Deleting this file is how the
//! aliases get dropped**: the compile errors it leaves behind are the complete
//! list of places that still honour an old name, `src/migrate.rs` among them.
//! That change comes a release after the move ships, so installs that update
//! have had their files renamed; the Node proxy's CA is still the hard one.
//!
//! The guard test at the bottom fails if the old name is written in code
//! anywhere else under `src/`, in any case and any shape (a literal, an
//! identifier, an environment variable), which is what keeps this file the
//! only one to delete.

/// The config file every install before the rename has:
/// `~/.config/teamclaude.json`. Used when `~/.config/tcr.json` does not exist
/// and this one does ([`crate::paths::config_file`]).
pub const CONFIG_FILE: &str = "teamclaude.json";

/// The cache directory every install before the rename has:
/// `<cache base>/teamclaude/`. Holds the usage ledger, the session-affinity
/// pins, the session-wire file, peer state and the logs, and is chosen as one
/// unit so an install never splits its cache across two folders
/// ([`crate::paths::cache_dir`]).
pub const CACHE_DIR: &str = "teamclaude";

/// The rotating log file prefix inside the old cache directory. It stays tied
/// to that directory because `tracing_appender` prunes by prefix: a new prefix
/// in the old directory would leave the old logs there forever.
pub const LOG_PREFIX: &str = "teamclaude-rs.log";

/// The Node `teamclaude` proxy's leaf certificate in `~/.config`. `tcr` reuses
/// it when present, because clients already trust the CA that signed it.
pub const JS_LEAF_CERT: &str = "teamclaude-leaf.pem";

/// The private key for [`JS_LEAF_CERT`].
pub const JS_LEAF_KEY: &str = "teamclaude-leaf.key";

/// The Node `teamclaude` proxy's CA, written next to its leaf. Reported as the
/// CA to trust when the reused leaf is served; `NODE_EXTRA_CA_CERTS` points at
/// it on machines that ran the JS proxy, so it can never move.
pub const JS_CA_CERT: &str = "teamclaude-ca.pem";

/// The Node `teamclaude` proxy's executable name, as it appears in a process's
/// argv. `tcr` recognises it to replace (`singleton`) or report (`doctor`) a
/// JS proxy still holding the port.
pub const JS_PROXY_BIN: &str = "teamclaude";

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    /// The old project name. Every name in this file contains it, and after the
    /// rename nothing else under `src/` may write it in code, in any case: not
    /// as a literal, an identifier, an environment variable or a crate path.
    /// Matched as a case-insensitive substring of code with comments removed.
    const OLD_NAME: &str = "teamclaude";

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

    /// Every `(line, code)` in `source` that writes [`OLD_NAME`] outside a
    /// comment, with the offending line trimmed.
    fn violations(source: &str) -> Vec<(usize, String)> {
        strip_comments(source)
            .lines()
            .enumerate()
            .filter(|(_, line)| line.to_ascii_lowercase().contains(OLD_NAME))
            .map(|(n, line)| (n + 1, line.trim().to_string()))
            .collect()
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

    /// The rule this file exists for: the old name is written in code nowhere
    /// under `src/` except here. Comments may mention it, because they explain
    /// what an existing install still has on disk.
    #[test]
    fn old_names_are_written_only_in_legacy_names() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let this_file = src.join("legacy_names.rs");
        let mut files = Vec::new();
        rust_files(&src, &mut files);
        assert!(
            files.contains(&this_file) && files.contains(&src.join("paths.rs")),
            "the walk must reach this file and paths.rs, found {} files",
            files.len()
        );
        let mut found = Vec::new();
        for file in files.iter().filter(|f| **f != this_file) {
            let source =
                std::fs::read_to_string(file).unwrap_or_else(|e| panic!("read {file:?}: {e}"));
            for (line, code) in violations(&source) {
                found.push(format!(
                    "{}:{line}: {code}",
                    file.strip_prefix(env!("CARGO_MANIFEST_DIR"))
                        .unwrap_or(file)
                        .display()
                ));
            }
        }
        assert!(
            found.is_empty(),
            "the old name belongs in src/legacy_names.rs only; use its constants:\n{}",
            found.join("\n")
        );
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

    /// Routing, not names: `src/paths.rs` is the only code that builds the
    /// `~/.config` and `~/.cache` bases. A `home.join(".cache").join("tcr")`
    /// written anywhere else picks the new name on an old install and passes
    /// the name guard above, which is exactly the split cache this rule exists
    /// to prevent. The walk covers `src/` and every crate under `crates/`; no
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
    }

    /// The positive control: the scanner flags the old name in every shape code
    /// can write it and leaves it alone in comments, so a green guard is not a
    /// scanner that sees nothing.
    #[test]
    fn the_guard_sees_code_and_ignores_comments() {
        let code = [
            format!("let a = \"{}\";", super::CONFIG_FILE),
            format!("let b = x == \"{}\";", super::JS_PROXY_BIN),
            format!("let c = r#\"{}.2026\"#;", super::LOG_PREFIX),
            "let d = \"/opt/nvm/bin/teamclaude\";".to_string(),
            "use teamclaude_rs::x;".to_string(),
            "const E: &str = \"TEAMCLAUDE_RS_INSTALL_DIR\";".to_string(),
            "enum K { Teamclaude }".to_string(),
        ]
        .join("\n");
        let lines: Vec<usize> = violations(&code).into_iter().map(|(n, _)| n).collect();
        assert_eq!(
            lines,
            vec![1, 2, 3, 4, 5, 6, 7],
            "each line holds one violation"
        );

        let comments = format!(
            "// {cfg}\n/// {cfg}\n/* {cfg}\n \"{bin}\" */\nlet url = \"https://x\"; // {cfg}\n",
            cfg = super::CONFIG_FILE,
            bin = super::JS_PROXY_BIN
        );
        assert_eq!(violations(&comments), Vec::new(), "comments are allowed");

        // A quote inside a char literal must not open a string that swallows
        // the comment marker after it.
        let tricky = format!("let q = '\"'; // {}\nlet r = 'a';\n", super::CONFIG_FILE);
        assert_eq!(violations(&tricky), Vec::new());
        assert_eq!(
            violations("use tcr::x; let p = \"/path/tcr\";\n"),
            Vec::new()
        );
    }
}
