//! Every on-disk and process name `tcr` inherited from `teamclaude`, in one file.
//!
//! The project is being renamed from `teamclaude-rs` to `tcr`. A fresh install
//! gets the new names, which live in [`crate::paths`]. An install that already
//! has the old files keeps using them **where they are**: nothing here is ever
//! moved, copied or symlinked. Two reasons, both expensive:
//!
//! - Users were told to `export NODE_EXTRA_CA_CERTS=<path to the CA>`, so the
//!   CA file can never move without breaking every client that trusts it.
//! - The config is written atomically (temp file plus rename), which replaces a
//!   symlink with a real file. A symlinked config would split in two on the
//!   first write.
//!
//! Each constant says why it still exists. **Deleting this file is how the
//! aliases get dropped**: the compile errors it leaves behind are the complete
//! list of places that still honour an old name. That later change has to
//! decide how existing installs move their files, and the CA is the hard one.
//!
//! The guard test at the bottom fails if one of these names is written as a
//! literal anywhere else under `src/`, which is what keeps this file the only
//! one to delete.

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

    /// Every name in this file that must not be written as a literal anywhere
    /// else under `src/`. Matched as substrings of code with comments removed.
    const NEEDLES: [&str; 4] = [
        super::CONFIG_FILE,
        "teamclaude-leaf",
        "teamclaude-ca",
        super::LOG_PREFIX,
    ];

    /// `"teamclaude"` as a whole string literal, quotes included.
    fn whole_literal() -> String {
        format!("\"{}\"", super::JS_PROXY_BIN)
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

    /// Every `(line, needle)` in `source` that breaks the rule.
    fn violations(source: &str) -> Vec<(usize, String)> {
        let whole = whole_literal();
        strip_comments(source)
            .lines()
            .enumerate()
            .flat_map(|(n, line)| {
                NEEDLES
                    .iter()
                    .map(|needle| (*needle).to_string())
                    .chain(std::iter::once(whole.clone()))
                    .filter(move |needle| line.contains(needle.as_str()))
                    .map(move |needle| (n + 1, needle))
            })
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

    /// The rule this file exists for: no old name is written as a literal in
    /// code anywhere under `src/` except here. Comments may mention them, and
    /// the crate name `teamclaude_rs` is not an on-disk name.
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
            for (line, needle) in violations(&source) {
                found.push(format!(
                    "{}:{line}: {needle}",
                    file.strip_prefix(env!("CARGO_MANIFEST_DIR"))
                        .unwrap_or(file)
                        .display()
                ));
            }
        }
        assert!(
            found.is_empty(),
            "old names belong in src/legacy_names.rs only; use its constants:\n{}",
            found.join("\n")
        );
    }

    /// The positive control: the scanner flags each needle in code and leaves
    /// it alone in comments, so a green guard is not a scanner that sees nothing.
    #[test]
    fn the_guard_sees_code_and_ignores_comments() {
        let code = format!(
            "let a = \"{}\";\nlet b = x == {};\nlet c = r#\"{}.2026\"#;\n",
            super::CONFIG_FILE,
            whole_literal(),
            super::LOG_PREFIX
        );
        let lines: Vec<usize> = violations(&code).into_iter().map(|(n, _)| n).collect();
        assert_eq!(lines, vec![1, 2, 3], "each line holds one violation");

        let comments = format!(
            "// {cfg}\n/// {cfg}\n/* {cfg}\n {whole} */\nlet url = \"https://x\"; // {cfg}\n",
            cfg = super::CONFIG_FILE,
            whole = whole_literal()
        );
        assert_eq!(violations(&comments), Vec::new(), "comments are allowed");

        // A quote inside a char literal must not open a string that swallows
        // the comment marker after it.
        let tricky = format!("let q = '\"'; // {}\nlet r = 'a';\n", super::CONFIG_FILE);
        assert_eq!(violations(&tricky), Vec::new());
        // `teamclaude_rs` and a longer literal containing the bin name are fine.
        assert_eq!(
            violations("use teamclaude_rs::x; let p = \"/path/teamclaude\";\n"),
            Vec::new()
        );
    }
}
