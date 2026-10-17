//! `tcr setup`: make typing `claude` go through tcr, in one of two ways.
//!
//! # The two routes
//!
//! 1. **Shell** (`tcr setup shell`). A marked block in the shell's startup
//!    file defines a `claude` function that runs `tcr run -- "$@"`, or the real
//!    `claude` when a `tcr run` is already above it (`TCR_RUN_ACTIVE`, set by
//!    `mark_run_active` in `src/main.rs`). `tcr run` already launches plain
//!    `claude` when the proxy is down, so a stopped proxy never breaks the
//!    command. It covers terminals and nothing else: an IDE extension or an app
//!    that starts `claude` itself never reads a shell function.
//! 2. **Every launch** (`tcr setup claude-settings`). One key,
//!    `env.ANTHROPIC_BASE_URL`, merged into `~/.claude/settings.json`, so every
//!    claude process routes, whoever started it. The cost: with tcr not
//!    running, claude cannot reach Anthropic at all until tcr starts or the key
//!    is removed.
//!
//! # Refuse, never overwrite
//!
//! A `claude` alias or function the user already has outside tcr's markers
//! decides what `claude` means, and in zsh an alias beats a function outright,
//! so writing the block anyway would install a route that silently does
//! nothing (or silently replaces the user's own wrapper). Both are refused,
//! naming the line found and its `file:line`. The same goes for a base URL in
//! the settings file that tcr did not write: refused unless `--force`.
//!
//! # Boundaries
//!
//! Everything here takes a [`Homes`] rather than reading `$HOME` itself, so a
//! test drives it against a scratch directory and never the operator's real
//! startup files. Edits stay between tcr's own markers (or, for the settings
//! file, on tcr's one key) and are undone byte for byte by the matching
//! `--remove`.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::doctor::{BASE_URL_VAR, routes_to_port};

/// The first line of tcr's block, matched as a line PREFIX so the hint after
/// it can change without stranding blocks written by an older build.
pub const START_MARKER: &str = "# >>> tcr >>>";

/// The whole first line as tcr writes it.
pub const START_LINE: &str = "# >>> tcr >>>  (added by tcr; remove with: tcr setup shell --remove)";

/// The last line of tcr's block, matched whole.
pub const END_MARKER: &str = "# <<< tcr <<<";

/// The shells `tcr setup shell` knows how to write a function for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Shell {
    Zsh,
    Bash,
    Fish,
}

impl Shell {
    pub const ALL: [Self; 3] = [Self::Zsh, Self::Bash, Self::Fish];

    /// Recognise a shell from a name or a path (`zsh`, `/bin/zsh`, `-zsh`).
    pub fn from_name(name: &str) -> Option<Self> {
        let base = name.rsplit('/').next().unwrap_or(name);
        match base.trim_start_matches('-') {
            "zsh" => Some(Self::Zsh),
            "bash" => Some(Self::Bash),
            "fish" => Some(Self::Fish),
            _ => None,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Zsh => "zsh",
            Self::Bash => "bash",
            Self::Fish => "fish",
        }
    }
}

/// Where the startup files live: `HOME`, zsh's `ZDOTDIR`, fish's
/// `XDG_CONFIG_HOME`, and whether this is macOS (where bash runs as a login
/// shell and reads `~/.bash_profile` rather than `~/.bashrc`).
#[derive(Debug, Clone)]
pub struct Homes {
    pub home: PathBuf,
    pub zdotdir: Option<PathBuf>,
    pub xdg_config: Option<PathBuf>,
    pub macos: bool,
}

impl Homes {
    /// This process's own: `HOME` (required, never guessed), `ZDOTDIR` and
    /// `XDG_CONFIG_HOME` when set and non-empty.
    pub fn from_env() -> Result<Self, SetupError> {
        let non_empty = |key: &str| {
            std::env::var_os(key)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        };
        let home = non_empty("HOME").ok_or(SetupError::NoHome)?;
        Ok(Self {
            home,
            zdotdir: non_empty("ZDOTDIR"),
            xdg_config: non_empty("XDG_CONFIG_HOME"),
            macos: cfg!(target_os = "macos"),
        })
    }

    fn zsh_dir(&self) -> &Path {
        self.zdotdir.as_deref().unwrap_or(&self.home)
    }

    fn fish_dir(&self) -> PathBuf {
        self.xdg_config
            .clone()
            .unwrap_or_else(|| self.home.join(".config"))
            .join("fish")
    }

    /// `~/.claude/settings.json`, the same file `tcr doctor` reads as the
    /// user's settings.
    pub fn claude_settings(&self) -> PathBuf {
        self.home.join(".claude").join("settings.json")
    }
}

/// Which route a conflict blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RouteKind {
    Shell,
    ClaudeSettings,
}

/// What was found in the way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ConflictKind {
    /// `alias claude=…` (or fish's `alias claude …`).
    Alias,
    /// `claude() {…}` or `function claude …`.
    Function,
    /// fish's `abbr … claude …`, which rewrites the typed word.
    Abbreviation,
    /// fish's `functions/claude.fish` exists and tcr did not write it.
    ForeignFile,
    /// `env.ANTHROPIC_BASE_URL` in the settings file names another URL.
    BaseUrl,
}

impl ConflictKind {
    const fn describe(self) -> &'static str {
        match self {
            Self::Alias => "an alias",
            Self::Function => "a function",
            Self::Abbreviation => "an abbreviation",
            Self::ForeignFile => "a claude function file tcr did not write",
            Self::BaseUrl => "another base URL",
        }
    }
}

/// One thing that already decides what `claude` does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Conflict {
    pub route: RouteKind,
    pub kind: ConflictKind,
    pub path: PathBuf,
    /// 1-based; `None` for a whole-file or whole-value conflict.
    pub line: Option<usize>,
    /// The line as found (or the value, for [`ConflictKind::BaseUrl`]).
    pub text: String,
}

impl std::fmt::Display for Conflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.line {
            Some(line) => write!(
                f,
                "{}:{line}: {} ({})",
                self.path.display(),
                self.text,
                self.kind.describe()
            ),
            None => write!(
                f,
                "{}: {} ({})",
                self.path.display(),
                self.text,
                self.kind.describe()
            ),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    #[error("HOME is not set, so there is no startup file to edit")]
    NoHome,
    #[error(
        "refusing to install the shell route: `claude` is already defined, and that definition would win over tcr's:\n{}\nremove or rename it, then run `tcr setup shell` again",
        list(.0)
    )]
    ShellConflict(Vec<Conflict>),
    #[error(
        "refusing to change {}: env.{BASE_URL_VAR} is already {value}, which is not this proxy. Pass --force to replace it",
        path.display()
    )]
    ForeignBaseUrl { path: PathBuf, value: String },
    #[error(
        "refusing to remove env.{BASE_URL_VAR} from {}: its value {value} is not one tcr writes",
        path.display()
    )]
    NotTcrsBaseUrl { path: PathBuf, value: String },
    #[error("{}: {why}", path.display())]
    Malformed { path: PathBuf, why: String },
    #[error("refusing to touch {}: it is {what}", path.display())]
    Occupied { path: PathBuf, what: String },
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

fn list(conflicts: &[Conflict]) -> String {
    conflicts
        .iter()
        .map(|conflict| format!("  {conflict}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// What one file went through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Change {
    Added,
    Updated,
    Unchanged,
    Removed,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileChange {
    pub path: PathBuf,
    pub change: Change,
}

// ---------------------------------------------------------------------------
// The shell route
// ---------------------------------------------------------------------------

/// Quote `program` as one shell word, or leave it bare when it is a plain
/// command name. Single quotes mean the same in POSIX shells and fish for a
/// string with no `'` or `\` in it, and those are refused rather than escaped
/// two different ways.
pub fn shell_word(program: &str) -> Option<String> {
    if program.contains(['\'', '\\', '\n']) {
        return None;
    }
    let plain = !program.is_empty()
        && program
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '+'));
    Some(if plain {
        program.to_string()
    } else {
        format!("'{program}'")
    })
}

/// tcr's block for `shell`, with no final newline. `tcr` is the shell word
/// that runs tcr (`tcr`, or a quoted path when tcr is not on `PATH`).
pub fn shell_block(shell: Shell, tcr: &str) -> String {
    match shell {
        Shell::Zsh | Shell::Bash => format!(
            "{START_LINE}\n\
             claude() {{\n  \
             if [ -n \"$TCR_RUN_ACTIVE\" ]; then command claude \"$@\"\n  \
             else {tcr} run -- \"$@\"; fi\n\
             }}\n\
             {END_MARKER}"
        ),
        Shell::Fish => format!(
            "{START_LINE}\n\
             function claude\n    \
             if test -n \"$TCR_RUN_ACTIVE\"\n        \
             command claude $argv\n    \
             else\n        \
             {tcr} run -- $argv\n    \
             end\n\
             end\n\
             {END_MARKER}"
        ),
    }
}

/// The file fish autoloads `claude` from. tcr owns it whole.
fn fish_function_file(homes: &Homes) -> PathBuf {
    homes.fish_dir().join("functions").join("claude.fish")
}

/// The login file macOS bash reads: the first of the three that exists, or
/// `~/.bash_profile` when none does.
fn bash_login_file(homes: &Homes) -> PathBuf {
    [".bash_profile", ".bash_login", ".profile"]
        .iter()
        .map(|name| homes.home.join(name))
        .find(|path| path.exists())
        .unwrap_or_else(|| homes.home.join(".bash_profile"))
}

/// Does this startup file already pull in `~/.bashrc`? Any non-comment line
/// naming `.bashrc` counts: the usual `[ -f ~/.bashrc ] && . ~/.bashrc` and
/// its many spellings all do.
fn sources_bashrc(text: &str) -> bool {
    text.lines()
        .any(|line| !line.trim_start().starts_with('#') && line.contains(".bashrc"))
}

/// The files tcr writes its block into for `shell`.
pub fn shell_targets(shell: Shell, homes: &Homes) -> Vec<PathBuf> {
    match shell {
        Shell::Zsh => vec![homes.zsh_dir().join(".zshrc")],
        Shell::Bash => {
            let mut targets = vec![homes.home.join(".bashrc")];
            if homes.macos {
                let login = bash_login_file(homes);
                let already =
                    std::fs::read_to_string(&login).is_ok_and(|text| sources_bashrc(&text));
                if !already {
                    targets.push(login);
                }
            }
            targets
        }
        Shell::Fish => vec![fish_function_file(homes)],
    }
}

/// Every startup file of `shell` that could define `claude` before or after
/// tcr's block.
fn shell_scan_files(shell: Shell, homes: &Homes) -> Vec<PathBuf> {
    match shell {
        Shell::Zsh => [".zshenv", ".zprofile", ".zshrc", ".zlogin"]
            .iter()
            .map(|name| homes.zsh_dir().join(name))
            .collect(),
        Shell::Bash => [
            ".bashrc",
            ".bash_profile",
            ".bash_login",
            ".profile",
            ".bash_aliases",
        ]
        .iter()
        .map(|name| homes.home.join(name))
        .collect(),
        Shell::Fish => {
            let dir = homes.fish_dir();
            let mut files = vec![dir.join("config.fish")];
            if let Ok(entries) = std::fs::read_dir(dir.join("conf.d")) {
                let mut conf: Vec<PathBuf> = entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .filter(|path| path.extension().is_some_and(|ext| ext == "fish"))
                    .collect();
                conf.sort();
                files.extend(conf);
            }
            files
        }
    }
}

/// Where tcr's block sits in `text`: the byte range from the start of the
/// start-marker line to the end of the end-marker line, its newline excluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BlockSpan {
    start: usize,
    end: usize,
}

/// Find tcr's block. `Err` when a start marker has no end marker after it: a
/// half-block is a hand edit tcr will not guess its way through.
fn find_block(text: &str) -> Result<Option<BlockSpan>, String> {
    let mut offset = 0;
    let mut start = None;
    for line in text.split_inclusive('\n') {
        let content = line.trim_end_matches(['\n', '\r']);
        match start {
            None if content.starts_with(START_MARKER) => start = Some(offset),
            Some(begin) if content == END_MARKER => {
                return Ok(Some(BlockSpan {
                    start: begin,
                    end: offset + content.len(),
                }));
            }
            _ => {}
        }
        offset += line.len();
    }
    match start {
        Some(_) => Err(format!(
            "has `{START_MARKER}` with no `{END_MARKER}` after it; fix the block by hand"
        )),
        None => Ok(None),
    }
}

/// `text` with `block` in it: replaced in place when tcr's block is already
/// there, appended otherwise.
///
/// Appending to a file with no final newline adds one before the block and
/// leaves the block itself without one, so [`without_block`] can tell the
/// separator was tcr's and take it back out.
fn with_block(text: &str, block: &str) -> Result<String, String> {
    if let Some(span) = find_block(text)? {
        let mut out = String::with_capacity(text.len() + block.len());
        out.push_str(&text[..span.start]);
        out.push_str(block);
        out.push_str(&text[span.end..]);
        return Ok(out);
    }
    Ok(if text.is_empty() {
        format!("{block}\n")
    } else if text.ends_with('\n') {
        format!("{text}{block}\n")
    } else {
        format!("{text}\n{block}")
    })
}

/// `text` with tcr's block taken out, the exact inverse of [`with_block`]'s
/// append. `None` when there is no block.
fn without_block(text: &str) -> Result<Option<String>, String> {
    let Some(span) = find_block(text)? else {
        return Ok(None);
    };
    let rest = &text[span.end..];
    let (cut_start, cut_end) = match rest.strip_prefix('\n') {
        Some(_) => (span.start, span.end + 1),
        // The block ends the file with no newline: tcr added the newline
        // before it, so that goes too.
        None if rest.is_empty() && span.start > 0 && text[..span.start].ends_with('\n') => {
            (span.start - 1, span.end)
        }
        None => (span.start, span.end),
    };
    Ok(Some(format!("{}{}", &text[..cut_start], &text[cut_end..])))
}

/// The tokens of one line, split on whitespace and shell separators.
fn tokens(line: &str) -> Vec<&str> {
    line.split(|c: char| c.is_whitespace() || matches!(c, ';' | '&' | '|'))
        .filter(|token| !token.is_empty())
        .collect()
}

fn unquote(token: &str) -> &str {
    token.trim_matches(['\'', '"'])
}

/// Does this POSIX-shell line define `claude`?
fn sh_conflict(line: &str) -> Option<ConflictKind> {
    let trimmed = line.trim_start();
    if trimmed.starts_with('#') {
        return None;
    }
    let words = tokens(trimmed);
    let aliases_claude = words
        .iter()
        .position(|word| *word == "alias")
        .is_some_and(|at| {
            words[at + 1..]
                .iter()
                .any(|word| unquote(word).starts_with("claude="))
        });
    if aliases_claude {
        return Some(ConflictKind::Alias);
    }
    let first = words.first().copied().unwrap_or_default();
    let second = words.get(1).copied().unwrap_or_default();
    let names_claude = |word: &str| word.split(['(', '{']).next() == Some("claude");
    if first.starts_with("claude(") || (first == "claude" && second.starts_with('(')) {
        return Some(ConflictKind::Function);
    }
    if first == "function" && names_claude(second) {
        return Some(ConflictKind::Function);
    }
    None
}

/// Does this fish line define `claude`?
fn fish_conflict(line: &str) -> Option<ConflictKind> {
    let trimmed = line.trim_start();
    if trimmed.starts_with('#') {
        return None;
    }
    let words = tokens(trimmed);
    let first = words.first().copied().unwrap_or_default();
    let mut rest = words.iter().skip(1).map(|word| unquote(word));
    match first {
        "alias" => rest
            .find(|word| !word.starts_with('-'))
            .filter(|word| *word == "claude" || word.starts_with("claude="))
            .map(|_| ConflictKind::Alias),
        "function" => rest
            .next()
            .filter(|word| *word == "claude")
            .map(|_| ConflictKind::Function),
        "abbr" => rest
            .find(|word| !word.starts_with('-'))
            .filter(|word| *word == "claude")
            .map(|_| ConflictKind::Abbreviation),
        _ => None,
    }
}

fn read_optional(path: &Path) -> Result<Option<String>, SetupError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(SetupError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Every line outside tcr's block, in every startup file of `shell`, that
/// already defines `claude`.
pub fn shell_conflicts(shell: Shell, homes: &Homes) -> Result<Vec<Conflict>, SetupError> {
    let mut found = Vec::new();
    for path in shell_scan_files(shell, homes) {
        let Some(text) = read_optional(&path)? else {
            continue;
        };
        let span = find_block(&text).map_err(|why| SetupError::Malformed {
            path: path.clone(),
            why,
        })?;
        let mut offset = 0;
        for (index, line) in text.split_inclusive('\n').enumerate() {
            let inside = span.is_some_and(|span| offset >= span.start && offset <= span.end);
            offset += line.len();
            if inside {
                continue;
            }
            let content = line.trim_end_matches(['\n', '\r']);
            let kind = match shell {
                Shell::Fish => fish_conflict(content),
                Shell::Zsh | Shell::Bash => sh_conflict(content),
            };
            if let Some(kind) = kind {
                found.push(Conflict {
                    route: RouteKind::Shell,
                    kind,
                    path: path.clone(),
                    line: Some(index + 1),
                    text: content.trim().to_string(),
                });
            }
        }
    }
    if shell == Shell::Fish {
        let path = fish_function_file(homes);
        let foreign = read_optional(&path)?
            .filter(|text| !text.lines().any(|line| line.starts_with(START_MARKER)));
        if let Some(text) = foreign {
            let first = text.lines().next().unwrap_or_default().trim().to_string();
            found.push(Conflict {
                route: RouteKind::Shell,
                kind: ConflictKind::ForeignFile,
                path,
                line: None,
                text: first,
            });
        }
    }
    Ok(found)
}

/// Write `text` to `path` without replacing a symlink with a file and without
/// leaving a half-written file behind: a temp file beside the real target,
/// then a rename. The target's permissions carry over.
fn write_file(path: &Path, text: &str) -> Result<(), SetupError> {
    let io = |source| SetupError::Io {
        path: path.to_path_buf(),
        source,
    };
    let target = match std::fs::canonicalize(path) {
        Ok(real) => real,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => path.to_path_buf(),
        Err(error) => return Err(io(error)),
    };
    let dir = target
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir).map_err(io)?;
    let permissions = std::fs::metadata(&target)
        .ok()
        .map(|meta| meta.permissions());
    let mut temp = tempfile::NamedTempFile::new_in(dir).map_err(io)?;
    std::io::Write::write_all(&mut temp, text.as_bytes()).map_err(io)?;
    if let Some(permissions) = permissions {
        temp.as_file().set_permissions(permissions).map_err(io)?;
    }
    temp.persist(&target).map_err(|error| io(error.error))?;
    Ok(())
}

/// Install the shell route for `shell`. Refuses, writing nothing, when any
/// startup file already defines `claude` outside tcr's block.
pub fn install_shell(
    shell: Shell,
    homes: &Homes,
    tcr: &str,
) -> Result<Vec<FileChange>, SetupError> {
    let conflicts = shell_conflicts(shell, homes)?;
    if !conflicts.is_empty() {
        return Err(SetupError::ShellConflict(conflicts));
    }
    let block = shell_block(shell, tcr);
    let mut changes = Vec::new();
    for path in shell_targets(shell, homes) {
        let before = read_optional(&path)?;
        let text = before.clone().unwrap_or_default();
        let had_block = find_block(&text)
            .map_err(|why| SetupError::Malformed {
                path: path.clone(),
                why,
            })?
            .is_some();
        let after = with_block(&text, &block).map_err(|why| SetupError::Malformed {
            path: path.clone(),
            why,
        })?;
        let change = if before.as_deref() == Some(after.as_str()) {
            Change::Unchanged
        } else if had_block {
            Change::Updated
        } else {
            Change::Added
        };
        if change != Change::Unchanged {
            write_file(&path, &after)?;
        }
        changes.push(FileChange { path, change });
    }
    Ok(changes)
}

/// Every file of every shell that could carry tcr's block, each once.
fn all_block_files(homes: &Homes) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for shell in Shell::ALL {
        for path in shell_scan_files(shell, homes)
            .into_iter()
            .chain(shell_targets(shell, homes))
        {
            if !files.contains(&path) {
                files.push(path);
            }
        }
    }
    let fish_file = fish_function_file(homes);
    if !files.contains(&fish_file) {
        files.push(fish_file);
    }
    files
}

/// Take tcr's block out of every startup file of every shell that has one.
/// Nothing else in any file changes. fish's function file, which tcr owns
/// whole, is deleted once the block is all it held.
pub fn remove_shell(homes: &Homes) -> Result<Vec<FileChange>, SetupError> {
    let fish_file = fish_function_file(homes);
    let mut changes = Vec::new();
    for path in all_block_files(homes) {
        let Some(text) = read_optional(&path)? else {
            continue;
        };
        let Some(after) = without_block(&text).map_err(|why| SetupError::Malformed {
            path: path.clone(),
            why,
        })?
        else {
            continue;
        };
        if path == fish_file && after.trim().is_empty() {
            std::fs::remove_file(&path).map_err(|source| SetupError::Io {
                path: path.clone(),
                source,
            })?;
            changes.push(FileChange {
                path,
                change: Change::Deleted,
            });
        } else {
            write_file(&path, &after)?;
            changes.push(FileChange {
                path,
                change: Change::Removed,
            });
        }
    }
    Ok(changes)
}

/// The shell route as it stands for one shell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellRouteStatus {
    /// Every file tcr writes for this shell carries the block.
    pub installed: bool,
    /// The files that carry it, of any shell.
    pub files: Vec<PathBuf>,
}

pub fn shell_status(shell: Option<Shell>, homes: &Homes) -> Result<ShellRouteStatus, SetupError> {
    let mut files = Vec::new();
    for path in all_block_files(homes) {
        let Some(text) = read_optional(&path)? else {
            continue;
        };
        if matches!(find_block(&text), Ok(Some(_))) {
            files.push(path);
        }
    }
    let installed = shell.is_some_and(|shell| {
        shell_targets(shell, homes)
            .iter()
            .all(|target| files.contains(target))
    });
    Ok(ShellRouteStatus { installed, files })
}

// ---------------------------------------------------------------------------
// The every-launch route: one key in ~/.claude/settings.json
// ---------------------------------------------------------------------------

/// The value tcr writes for `port`.
pub fn proxy_base_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

/// Is this a value tcr writes, for any port? The only values `--remove` takes
/// out: anything else is the user's.
pub fn is_tcr_base_url(url: &str) -> bool {
    url.strip_prefix("http://127.0.0.1:")
        .is_some_and(|port| port.parse::<u16>().is_ok())
}

/// One `"key": value` member of a JSON object, by byte offsets.
#[derive(Debug, Clone)]
struct Member {
    key: String,
    /// The key's opening quote.
    start: usize,
    /// One past the key's closing quote.
    key_end: usize,
    value_start: usize,
    value_end: usize,
    /// The comma after the value, when there is one.
    comma: Option<usize>,
}

#[derive(Debug, Clone)]
struct ObjectSpan {
    open: usize,
    members: Vec<Member>,
}

const fn skip_ws(bytes: &[u8], mut at: usize) -> usize {
    while at < bytes.len() && matches!(bytes[at], b' ' | b'\t' | b'\n' | b'\r') {
        at += 1;
    }
    at
}

/// One past the closing quote of the string opening at `at`.
fn string_end(bytes: &[u8], at: usize) -> Result<usize, String> {
    if bytes.get(at) != Some(&b'"') {
        return Err(format!("expected a string at byte {at}"));
    }
    let mut i = at + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return Ok(i + 1),
            _ => i += 1,
        }
    }
    Err("unterminated string".to_string())
}

/// One past the end of the JSON value starting at `at`.
fn value_end(bytes: &[u8], at: usize) -> Result<usize, String> {
    match bytes.get(at) {
        Some(b'"') => string_end(bytes, at),
        Some(b'{' | b'[') => {
            let mut depth = 0usize;
            let mut i = at;
            while i < bytes.len() {
                match bytes[i] {
                    b'"' => {
                        i = string_end(bytes, i)?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return Ok(i + 1);
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            Err("unterminated object or array".to_string())
        }
        Some(_) => {
            let mut i = at;
            while i < bytes.len()
                && !matches!(bytes[i], b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r')
            {
                i += 1;
            }
            Ok(i)
        }
        None => Err("expected a value".to_string()),
    }
}

/// The members of the object whose `{` is at `open`.
fn scan_object(text: &str, open: usize) -> Result<ObjectSpan, String> {
    let bytes = text.as_bytes();
    if bytes.get(open) != Some(&b'{') {
        return Err(format!("expected an object at byte {open}"));
    }
    let mut members = Vec::new();
    let mut at = skip_ws(bytes, open + 1);
    if bytes.get(at) == Some(&b'}') {
        return Ok(ObjectSpan { open, members });
    }
    loop {
        let start = at;
        let key_end = string_end(bytes, start)?;
        let key: String =
            serde_json::from_str(&text[start..key_end]).map_err(|error| error.to_string())?;
        at = skip_ws(bytes, key_end);
        if bytes.get(at) != Some(&b':') {
            return Err(format!("expected `:` at byte {at}"));
        }
        let value_start = skip_ws(bytes, at + 1);
        let end = value_end(bytes, value_start)?;
        at = skip_ws(bytes, end);
        let comma = (bytes.get(at) == Some(&b',')).then_some(at);
        members.push(Member {
            key,
            start,
            key_end,
            value_start,
            value_end: end,
            comma,
        });
        match bytes.get(at) {
            Some(b',') => at = skip_ws(bytes, at + 1),
            Some(b'}') => return Ok(ObjectSpan { open, members }),
            _ => return Err(format!("expected `,` or `}}` at byte {at}")),
        }
    }
}

fn top_object(text: &str) -> Result<ObjectSpan, String> {
    let open = skip_ws(text.as_bytes(), 0);
    scan_object(text, open)
}

/// The byte range that removes member `index` of `object`, with exactly the
/// separator it brought: for the last of several, the comma before it; for any
/// other, the comma after it. The inverse of how [`insert_member_text`] and
/// [`with_base_url_text`] add one.
fn member_cut(object: &ObjectSpan, index: usize) -> Result<(usize, usize), String> {
    let member = &object.members[index];
    let count = object.members.len();
    if count == 1 {
        return Ok((object.open + 1, member.value_end));
    }
    if index + 1 == count {
        let previous = &object.members[index - 1];
        return Ok((previous.value_end, member.value_end));
    }
    let start = match index {
        0 => object.open + 1,
        _ => object.members[index - 1]
            .comma
            .map(|comma| comma + 1)
            .ok_or("a member before the last has no comma")?,
    };
    let end = member
        .comma
        .map(|comma| comma + 1)
        .ok_or("a member before the last has no comma")?;
    Ok((start, end))
}

/// The whitespace between `{` and the first member, and the `:` separator the
/// first member uses, so a new member matches what is already there.
fn layout(text: &str, object: &ObjectSpan) -> Option<(String, String)> {
    let first = object.members.first()?;
    Some((
        text[object.open + 1..first.start].to_string(),
        text[first.key_end..first.value_start].to_string(),
    ))
}

/// `text` with a new first member `"key"<sep>value` in a non-empty object.
fn insert_member_text(text: &str, object: &ObjectSpan, member: &str) -> Option<String> {
    let (lead, _) = layout(text, object)?;
    let at = object.open + 1;
    Some(format!("{}{lead}{member},{}", &text[..at], &text[at..]))
}

fn json_string(value: &str) -> String {
    serde_json::Value::String(value.to_string()).to_string()
}

/// `text` with `env.ANTHROPIC_BASE_URL` set to `url`, every other byte kept
/// where JSON allows.
fn with_base_url_text(text: &str, url: &str) -> Result<String, String> {
    let key = json_string(BASE_URL_VAR);
    let value = json_string(url);
    if text.trim().is_empty() {
        return Ok(format!("{{\n  \"env\": {{\n    {key}: {value}\n  }}\n}}\n"));
    }
    let top = top_object(text)?;
    if let Some(env) = top.members.iter().find(|member| member.key == "env") {
        let env_object = scan_object(text, env.value_start)?;
        if let Some(existing) = env_object.members.iter().find(|m| m.key == BASE_URL_VAR) {
            return Ok(format!(
                "{}{value}{}",
                &text[..existing.value_start],
                &text[existing.value_end..]
            ));
        }
        let sep = layout(text, &env_object).map_or_else(|| ": ".to_string(), |(_, sep)| sep);
        if let Some(out) = insert_member_text(text, &env_object, &format!("{key}{sep}{value}")) {
            return Ok(out);
        }
        // `"env": {}`: write it out at the env key's own indent.
        let indent = line_indent(text, env.start);
        let unit = indent_unit(text, &top);
        return Ok(format!(
            "{}{{\n{indent}{unit}{key}{sep}{value}\n{indent}}}{}",
            &text[..env.value_start],
            &text[env.value_end..]
        ));
    }
    if let Some((lead, sep)) = layout(text, &top) {
        let last = top
            .members
            .last()
            .ok_or("an object with a layout has a member")?;
        let addition = if lead.contains('\n') {
            let indent = lead.rsplit('\n').next().unwrap_or_default();
            format!(",{lead}\"env\"{sep}{{\n{indent}{indent}{key}{sep}{value}\n{indent}}}")
        } else {
            format!(",{lead}\"env\"{sep}{{{key}{sep}{value}}}")
        };
        Ok(format!(
            "{}{addition}{}",
            &text[..last.value_end],
            &text[last.value_end..]
        ))
    } else {
        let close = value_end(text.as_bytes(), top.open)?;
        Ok(format!(
            "{}{{\n  \"env\": {{\n    {key}: {value}\n  }}\n}}{}",
            &text[..top.open],
            &text[close..]
        ))
    }
}

/// The whitespace at the start of the line holding byte `at`.
fn line_indent(text: &str, at: usize) -> String {
    let line_start = text[..at].rfind('\n').map_or(0, |i| i + 1);
    text[line_start..at]
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect()
}

/// One level of indent, read off the top object's first member; two spaces
/// when the file gives no answer.
fn indent_unit(text: &str, top: &ObjectSpan) -> String {
    layout(text, top)
        .filter(|(lead, _)| lead.contains('\n'))
        .and_then(|(lead, _)| lead.rsplit('\n').next().map(str::to_string))
        .filter(|unit| !unit.is_empty())
        .unwrap_or_else(|| "  ".to_string())
}

/// `text` with `env.ANTHROPIC_BASE_URL` removed, and `env` with it when that
/// was its only key.
fn without_base_url_text(text: &str) -> Result<String, String> {
    let top = top_object(text)?;
    let Some(env_index) = top.members.iter().position(|member| member.key == "env") else {
        return Ok(text.to_string());
    };
    let env_object = scan_object(text, top.members[env_index].value_start)?;
    let Some(index) = env_object
        .members
        .iter()
        .position(|member| member.key == BASE_URL_VAR)
    else {
        return Ok(text.to_string());
    };
    let (start, end) = if env_object.members.len() == 1 {
        member_cut(&top, env_index)?
    } else {
        member_cut(&env_object, index)?
    };
    Ok(format!("{}{}", &text[..start], &text[end..]))
}

/// The settings file's current `env.ANTHROPIC_BASE_URL`, after checking the
/// file is an object whose `env` (if any) is an object too.
fn settings_document(path: &Path, text: &str) -> Result<serde_json::Value, SetupError> {
    let malformed = |why: String| SetupError::Malformed {
        path: path.to_path_buf(),
        why,
    };
    let document: serde_json::Value = if text.trim().is_empty() {
        serde_json::Value::Object(serde_json::Map::new())
    } else {
        serde_json::from_str(text)
            .map_err(|error| malformed(format!("not readable as JSON: {error}")))?
    };
    let Some(object) = document.as_object() else {
        return Err(malformed("is not a JSON object".to_string()));
    };
    match object.get("env") {
        None | Some(serde_json::Value::Object(_)) => {}
        Some(other) => return Err(malformed(format!("env is {other}, which is not an object"))),
    }
    let not_a_string = object
        .get("env")
        .and_then(|env| env.get(BASE_URL_VAR))
        .filter(|value| !value.is_string());
    if let Some(value) = not_a_string {
        return Err(malformed(format!(
            "env.{BASE_URL_VAR} is {value}, which is not a string"
        )));
    }
    Ok(document)
}

fn base_url_of(document: &serde_json::Value) -> Option<&str> {
    document.get("env")?.get(BASE_URL_VAR)?.as_str()
}

/// Write `after` only if it parses to exactly `expected`: the text surgery
/// above is checked against the meaning it was meant to have, and a mismatch
/// writes nothing.
fn write_checked(path: &Path, after: &str, expected: &serde_json::Value) -> Result<(), SetupError> {
    let parsed: serde_json::Value =
        serde_json::from_str(after).map_err(|error| SetupError::Malformed {
            path: path.to_path_buf(),
            why: format!("tcr's edit did not parse ({error}); nothing was written"),
        })?;
    if &parsed != expected {
        return Err(SetupError::Malformed {
            path: path.to_path_buf(),
            why: "tcr's edit changed more than its one key; nothing was written".to_string(),
        });
    }
    write_file(path, after)
}

/// Install the every-launch route: `env.ANTHROPIC_BASE_URL` pointing at this
/// proxy's `port`. Refuses another URL already there unless `force`.
pub fn install_settings(homes: &Homes, port: u16, force: bool) -> Result<FileChange, SetupError> {
    let path = homes.claude_settings();
    let text = read_optional(&path)?.unwrap_or_default();
    let document = settings_document(&path, &text)?;
    let want = proxy_base_url(port);
    let existed = match base_url_of(&document) {
        Some(current) if current == want => {
            return Ok(FileChange {
                path,
                change: Change::Unchanged,
            });
        }
        Some(current) if !force => {
            return Err(SetupError::ForeignBaseUrl {
                path,
                value: current.to_string(),
            });
        }
        Some(_) => true,
        None => false,
    };
    let after = with_base_url_text(&text, &want).map_err(|why| SetupError::Malformed {
        path: path.clone(),
        why,
    })?;
    let mut expected = document;
    if let Some(object) = expected.as_object_mut() {
        let env = object
            .entry("env")
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
        if let Some(env) = env.as_object_mut() {
            env.insert(BASE_URL_VAR.to_string(), serde_json::Value::String(want));
        }
    }
    write_checked(&path, &after, &expected)?;
    Ok(FileChange {
        path,
        change: if existed {
            Change::Updated
        } else {
            Change::Added
        },
    })
}

/// Take tcr's key back out. Refuses a value tcr does not write.
pub fn remove_settings(homes: &Homes) -> Result<FileChange, SetupError> {
    let path = homes.claude_settings();
    let Some(text) = read_optional(&path)? else {
        return Ok(FileChange {
            path,
            change: Change::Unchanged,
        });
    };
    let document = settings_document(&path, &text)?;
    match base_url_of(&document) {
        None => {
            return Ok(FileChange {
                path,
                change: Change::Unchanged,
            });
        }
        Some(value) if !is_tcr_base_url(value) => {
            return Err(SetupError::NotTcrsBaseUrl {
                path,
                value: value.to_string(),
            });
        }
        Some(_) => {}
    }
    let after = without_base_url_text(&text).map_err(|why| SetupError::Malformed {
        path: path.clone(),
        why,
    })?;
    let mut expected = document;
    if let Some(object) = expected.as_object_mut() {
        let now_empty = object
            .get_mut("env")
            .and_then(serde_json::Value::as_object_mut)
            .is_some_and(|env| {
                env.remove(BASE_URL_VAR);
                env.is_empty()
            });
        if now_empty {
            object.remove("env");
        }
    }
    write_checked(&path, &after, &expected)?;
    Ok(FileChange {
        path,
        change: Change::Removed,
    })
}

/// The every-launch route as it stands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsRouteStatus {
    /// The settings file's base URL reaches this proxy.
    pub installed: bool,
    pub path: PathBuf,
    /// What `env.ANTHROPIC_BASE_URL` says, if anything.
    pub base_url: Option<String>,
    /// Why the file could not be read, when it could not.
    pub problem: Option<String>,
}

pub fn settings_status(homes: &Homes, port: u16) -> SettingsRouteStatus {
    let path = homes.claude_settings();
    let read = read_optional(&path).and_then(|text| {
        let text = text.unwrap_or_default();
        settings_document(&path, &text).map(|document| base_url_of(&document).map(str::to_string))
    });
    match read {
        Ok(base_url) => SettingsRouteStatus {
            installed: base_url
                .as_deref()
                .is_some_and(|url| routes_to_port(url, port)),
            path,
            base_url,
            problem: None,
        },
        Err(error) => SettingsRouteStatus {
            installed: false,
            path,
            base_url: None,
            problem: Some(error.to_string()),
        },
    }
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

/// Which of the two routes is on, as one word for the panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Route {
    Off,
    Shell,
    EveryLaunch,
    Both,
}

impl Route {
    const fn of(shell: bool, settings: bool) -> Self {
        match (shell, settings) {
            (false, false) => Self::Off,
            (true, false) => Self::Shell,
            (false, true) => Self::EveryLaunch,
            (true, true) => Self::Both,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Shell => "shell",
            Self::EveryLaunch => "everyLaunch",
            Self::Both => "both",
        }
    }
}

/// `tcr setup status`: both routes and anything in the way of either.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub route: Route,
    /// The shell checked: `$SHELL`, or `--shell`. `None` when neither names a
    /// shell tcr can write for.
    pub shell: Option<Shell>,
    pub shell_route: ShellRouteStatus,
    pub claude_settings: SettingsRouteStatus,
    pub conflicts: Vec<Conflict>,
    pub port: u16,
}

pub fn status(shell: Option<Shell>, homes: &Homes, port: u16) -> Result<Status, SetupError> {
    let shell_route = shell_status(shell, homes)?;
    let claude_settings = settings_status(homes, port);
    let mut conflicts = match shell {
        Some(shell) => shell_conflicts(shell, homes)?,
        None => Vec::new(),
    };
    let foreign = claude_settings
        .base_url
        .as_deref()
        .filter(|url| !routes_to_port(url, port));
    if let Some(url) = foreign {
        conflicts.push(Conflict {
            route: RouteKind::ClaudeSettings,
            kind: ConflictKind::BaseUrl,
            path: claude_settings.path.clone(),
            line: None,
            text: url.to_string(),
        });
    }
    Ok(Status {
        route: Route::of(shell_route.installed, claude_settings.installed),
        shell,
        shell_route,
        claude_settings,
        conflicts,
        port,
    })
}

impl Status {
    /// Greppable `key: value` lines.
    pub fn render_text(&self) -> String {
        let mut lines = vec![format!("route: {}", self.route.label())];
        lines.push(format!(
            "shell: {}",
            self.shell.map_or("unknown (pass --shell)", Shell::name)
        ));
        let files = self
            .shell_route
            .files
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(match (self.shell_route.installed, files.is_empty()) {
            (true, _) => format!("shellRoute: installed ({files})"),
            (false, true) => "shellRoute: not installed".to_string(),
            (false, false) => {
                format!("shellRoute: not installed for this shell (block in {files})")
            }
        });
        let settings = &self.claude_settings;
        lines.push(match (&settings.base_url, settings.installed) {
            (Some(url), true) => format!(
                "claudeSettings: installed ({} sets {BASE_URL_VAR}={url})",
                settings.path.display()
            ),
            _ => "claudeSettings: not installed".to_string(),
        });
        if let Some(problem) = &settings.problem {
            lines.push(format!("problem: {problem}"));
        }
        for conflict in &self.conflicts {
            lines.push(format!("conflict: {conflict}"));
        }
        lines.join("\n")
    }
}

// ---------------------------------------------------------------------------
// What the binary reads from its own process
// ---------------------------------------------------------------------------

/// The shell to set up: `--shell` when given, else `$SHELL`, else zsh on
/// macOS (its default login shell since 10.15). `None` when none of those
/// names a shell tcr can write for.
pub fn chosen_shell(flag: Option<Shell>) -> Option<Shell> {
    if flag.is_some() {
        return flag;
    }
    match std::env::var("SHELL") {
        Ok(value) if !value.is_empty() => Shell::from_name(&value),
        _ if cfg!(target_os = "macos") => Some(Shell::Zsh),
        _ => None,
    }
}

/// The port the tcr config names, or the default when there is no config
/// yet. Never creates the file: `tcr setup` is a question about the shell and
/// the settings file, and a first run that asks it should not leave a config
/// behind as a side effect.
pub fn configured_port(path: &Path) -> anyhow::Result<u16> {
    match crate::config::load(path) {
        Ok(config) => Ok(config.proxy.port),
        Err(crate::config::ConfigError::Io(error))
            if error.kind() == std::io::ErrorKind::NotFound =>
        {
            let defaults: crate::config::Config = serde_json::from_str("{}")?;
            Ok(defaults.proxy.port)
        }
        Err(error) => Err(anyhow::anyhow!(
            "config at {} is unreadable: {error}",
            path.display()
        )),
    }
}

/// How the shell block names tcr: `tcr` when a `tcr` is on this process's
/// `PATH`, otherwise this binary's own path. TcrBar runs its BUNDLED tcr
/// (`TcrTool.swift`), which is often on no `PATH` at all, and a block that
/// said `tcr` would then fail in every terminal.
pub fn tcr_word() -> anyhow::Result<String> {
    let on_path = std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| {
            std::fs::metadata(dir.join("tcr")).is_ok_and(|meta| {
                use std::os::unix::fs::PermissionsExt as _;
                meta.is_file() && meta.permissions().mode() & 0o111 != 0
            })
        })
    });
    if on_path {
        return Ok("tcr".to_string());
    }
    let exe = std::env::current_exe()?;
    let exe = exe.to_string_lossy();
    shell_word(&exe).ok_or_else(|| {
        anyhow::anyhow!(
            "tcr is not on PATH and its own path ({exe}) cannot be quoted for a shell; put tcr on PATH and retry"
        )
    })
}

// ---------------------------------------------------------------------------
// tcr-run: `tcr run` under another name
// ---------------------------------------------------------------------------

/// The name `tcr setup tcr-run` links. The binary dispatches on it in
/// `src/main.rs` (`effective_argv`).
pub const TCR_RUN_LINK: &str = "tcr-run";

/// The directory of the first executable `tcr` on `path` (a `PATH` value).
pub fn tcr_dir_on_path(path: &std::ffi::OsStr) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;
    std::env::split_paths(path).find(|dir| {
        std::fs::metadata(dir.join("tcr"))
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    })
}

/// Is `link` a symlink tcr made: one whose target's file name is `tcr`?
fn is_tcr_link(link: &Path) -> bool {
    std::fs::read_link(link).is_ok_and(|target| target.file_name().is_some_and(|n| n == "tcr"))
}

/// Make `<dir>/tcr-run` a symlink to the `tcr` beside it. The target is the
/// relative name `tcr`, so the link follows whatever `tcr` that directory
/// holds after an upgrade. Refuses anything already at that path that is not
/// such a link: a file, or a symlink to something else, is someone's own.
pub fn install_tcr_run(dir: &Path) -> Result<FileChange, SetupError> {
    let link = dir.join(TCR_RUN_LINK);
    match std::fs::symlink_metadata(&link) {
        Ok(_) if is_tcr_link(&link) => {
            return Ok(FileChange {
                path: link,
                change: Change::Unchanged,
            });
        }
        Ok(meta) => {
            let what = if meta.file_type().is_symlink() {
                format!(
                    "a symlink to {}",
                    std::fs::read_link(&link)
                        .map(|target| target.display().to_string())
                        .unwrap_or_default()
                )
            } else {
                "a file tcr did not make".to_string()
            };
            return Err(SetupError::Occupied { path: link, what });
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => return Err(SetupError::Io { path: link, source }),
    }
    std::os::unix::fs::symlink("tcr", &link).map_err(|source| SetupError::Io {
        path: link.clone(),
        source,
    })?;
    Ok(FileChange {
        path: link,
        change: Change::Added,
    })
}

/// Take `<dir>/tcr-run` out, only when it is the link tcr made.
pub fn remove_tcr_run(dir: &Path) -> Result<FileChange, SetupError> {
    let link = dir.join(TCR_RUN_LINK);
    match std::fs::symlink_metadata(&link) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(FileChange {
            path: link,
            change: Change::Unchanged,
        }),
        Err(source) => Err(SetupError::Io { path: link, source }),
        Ok(_) if is_tcr_link(&link) => {
            std::fs::remove_file(&link).map_err(|source| SetupError::Io {
                path: link.clone(),
                source,
            })?;
            Ok(FileChange {
                path: link,
                change: Change::Deleted,
            })
        }
        Ok(_) => Err(SetupError::Occupied {
            path: link,
            what: "not the symlink tcr makes".to_string(),
        }),
    }
}

/// One line per file touched, for the person who ran the verb.
pub fn describe_changes(changes: &[FileChange]) -> String {
    if changes.is_empty() {
        return "nothing to change".to_string();
    }
    changes
        .iter()
        .map(|change| {
            let verb = match change.change {
                Change::Added => "added tcr's block to",
                Change::Updated => "updated tcr's block in",
                Change::Unchanged => "already set up:",
                Change::Removed => "removed tcr's block from",
                Change::Deleted => "deleted",
            };
            format!("{verb} {}", change.path.display())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn homes(dir: &Path) -> Homes {
        Homes {
            home: dir.to_path_buf(),
            zdotdir: None,
            xdg_config: None,
            macos: true,
        }
    }

    #[test]
    fn with_and_without_block_are_inverses() {
        let block = shell_block(Shell::Zsh, "tcr");
        for original in ["", "a\n", "a", "a\n\nb\n", "a\nb"] {
            let added = with_block(original, &block).expect("add");
            assert!(added.contains(&block), "{added:?}");
            assert_eq!(with_block(&added, &block).expect("re-add"), added);
            let removed = without_block(&added).expect("remove").expect("a block");
            assert_eq!(removed, original, "from {original:?} via {added:?}");
        }
    }

    #[test]
    fn a_block_in_the_middle_comes_out_cleanly() {
        let block = shell_block(Shell::Bash, "tcr");
        let text = format!("one\n{block}\ntwo\n");
        assert_eq!(
            without_block(&text).expect("ok").expect("found"),
            "one\ntwo\n"
        );
    }

    #[test]
    fn an_unclosed_block_is_refused() {
        assert!(find_block(&format!("{START_LINE}\nclaude() {{ :; }}\n")).is_err());
    }

    #[test]
    fn sh_conflicts_cover_the_spellings() {
        for line in [
            "alias claude='claude --x'",
            "alias claude=\"x\"",
            "  alias -g claude=x",
            "alias ll=ls claude=x",
            "[ -x foo ] && alias claude=foo",
        ] {
            assert_eq!(sh_conflict(line), Some(ConflictKind::Alias), "{line}");
        }
        for line in [
            "claude() {",
            "claude () {",
            "function claude {",
            "function claude() {",
            "claude(){ :; }",
        ] {
            assert_eq!(sh_conflict(line), Some(ConflictKind::Function), "{line}");
        }
        for line in [
            "# alias claude=x",
            "alias claudex=y",
            "export CLAUDE_CODE=1",
            "claude --version",
            "function claudette {",
            "echo claude()",
        ] {
            assert_eq!(sh_conflict(line), None, "{line}");
        }
    }

    #[test]
    fn fish_conflicts_cover_the_spellings() {
        assert_eq!(fish_conflict("alias claude 'x'"), Some(ConflictKind::Alias));
        assert_eq!(fish_conflict("alias claude=x"), Some(ConflictKind::Alias));
        assert_eq!(
            fish_conflict("function claude"),
            Some(ConflictKind::Function)
        );
        assert_eq!(
            fish_conflict("function claude --wraps x"),
            Some(ConflictKind::Function)
        );
        assert_eq!(
            fish_conflict("abbr -a claude 'tcr run'"),
            Some(ConflictKind::Abbreviation)
        );
        assert_eq!(fish_conflict("abbr -a cl claude"), None);
        assert_eq!(fish_conflict("function claudette"), None);
    }

    #[test]
    fn the_shell_word_quotes_only_when_it_must() {
        assert_eq!(shell_word("tcr").as_deref(), Some("tcr"));
        assert_eq!(
            shell_word("/Applications/TcrBar.app/Contents/MacOS/tcr").as_deref(),
            Some("/Applications/TcrBar.app/Contents/MacOS/tcr")
        );
        assert_eq!(shell_word("/a b/tcr").as_deref(), Some("'/a b/tcr'"));
        assert_eq!(shell_word("/it's/tcr"), None);
    }

    fn round_trip(original: &str) {
        let added = with_base_url_text(original, "http://127.0.0.1:4100").expect("add");
        let parsed: serde_json::Value = serde_json::from_str(&added).expect("still JSON");
        assert_eq!(
            parsed["env"][BASE_URL_VAR], "http://127.0.0.1:4100",
            "{added}"
        );
        let removed = without_base_url_text(&added).expect("remove");
        assert_eq!(removed, original, "via {added}");
    }

    #[test]
    fn settings_text_round_trips_in_every_shape() {
        round_trip("{\n  \"model\": \"opus\"\n}\n");
        round_trip("{\n  \"env\": {\n    \"FOO\": \"bar\"\n  },\n  \"model\": \"opus\"\n}\n");
        round_trip("{\"model\":\"opus\"}");
        round_trip("{\"env\":{\"A\":\"1\",\"B\":\"2\"}}");
        round_trip("{\n\t\"a\": [1, {\"b\": \"}\"}],\n\t\"c\": null\n}\n");
    }

    #[test]
    fn an_empty_env_and_an_empty_file_get_a_written_out_block() {
        let added = with_base_url_text("{\n  \"env\": {}\n}\n", "http://127.0.0.1:1").expect("add");
        assert_eq!(
            added,
            "{\n  \"env\": {\n    \"ANTHROPIC_BASE_URL\": \"http://127.0.0.1:1\"\n  }\n}\n"
        );
        let fresh = with_base_url_text("", "http://127.0.0.1:1").expect("add");
        assert_eq!(fresh, added);
        let empty = with_base_url_text("{}\n", "http://127.0.0.1:1").expect("add");
        assert_eq!(empty, added);
    }

    #[test]
    fn an_existing_value_is_replaced_in_place() {
        let text = "{\"env\": {\"ANTHROPIC_BASE_URL\": \"https://x.example\", \"Z\": \"1\"}}";
        let out = with_base_url_text(text, "http://127.0.0.1:1").expect("replace");
        assert_eq!(
            out,
            "{\"env\": {\"ANTHROPIC_BASE_URL\": \"http://127.0.0.1:1\", \"Z\": \"1\"}}"
        );
    }

    #[test]
    fn remove_only_takes_values_tcr_writes() {
        assert!(is_tcr_base_url("http://127.0.0.1:3456"));
        assert!(!is_tcr_base_url("http://127.0.0.1:3456/v1"));
        assert!(!is_tcr_base_url("https://gateway.example:9443"));
    }

    #[test]
    fn bash_on_macos_writes_the_login_file_when_it_does_not_source_bashrc() {
        let dir = tempfile::tempdir().expect("scratch");
        let homes = homes(dir.path());
        std::fs::write(dir.path().join(".profile"), "export A=1\n").expect("seed");
        let targets = shell_targets(Shell::Bash, &homes);
        assert_eq!(
            targets,
            vec![dir.path().join(".bashrc"), dir.path().join(".profile")],
            "the first existing login file is the one bash reads"
        );
        let linux = Homes {
            macos: false,
            ..homes
        };
        assert_eq!(
            shell_targets(Shell::Bash, &linux),
            vec![dir.path().join(".bashrc")]
        );
    }

    #[test]
    fn zdotdir_moves_the_zsh_files() {
        let dir = tempfile::tempdir().expect("scratch");
        let homes = Homes {
            zdotdir: Some(dir.path().join("zdot")),
            ..homes(dir.path())
        };
        assert_eq!(
            shell_targets(Shell::Zsh, &homes),
            vec![dir.path().join("zdot").join(".zshrc")]
        );
    }

    #[test]
    fn a_symlinked_rc_stays_a_symlink() {
        let dir = tempfile::tempdir().expect("scratch");
        let real = dir.path().join("dotfiles-zshrc");
        std::fs::write(&real, "setopt autocd\n").expect("seed");
        std::os::unix::fs::symlink(&real, dir.path().join(".zshrc")).expect("link");
        let homes = homes(dir.path());
        install_shell(Shell::Zsh, &homes, "tcr").expect("install");
        let link = dir.path().join(".zshrc");
        assert!(
            std::fs::symlink_metadata(&link)
                .expect("meta")
                .file_type()
                .is_symlink()
        );
        assert!(
            std::fs::read_to_string(&real)
                .expect("read")
                .contains(START_MARKER)
        );
        remove_shell(&homes).expect("remove");
        assert_eq!(
            std::fs::read_to_string(&real).expect("read"),
            "setopt autocd\n"
        );
    }

    #[test]
    fn a_conflict_inside_tcrs_own_block_is_not_a_conflict() {
        let dir = tempfile::tempdir().expect("scratch");
        let homes = homes(dir.path());
        install_shell(Shell::Zsh, &homes, "tcr").expect("install");
        assert!(
            shell_conflicts(Shell::Zsh, &homes)
                .expect("scan")
                .is_empty()
        );
        install_shell(Shell::Zsh, &homes, "tcr").expect("re-install is not refused by itself");
    }
}
