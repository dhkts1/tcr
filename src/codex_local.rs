//! What this Mac holds for codex: the `codex` binary, whether codex has a login of its own, and
//! any older `tcr` that would misread a config holding a Codex account.
//!
//! Nothing here reads a credential or runs `codex`. Every check on a codex file is an
//! existence check, never a read, and that is deliberate for each one:
//!
//! - **The login** ([`codex_login_path`], [`codex_login_exists`]). Codex keeps its ChatGPT login at
//!   `$CODEX_HOME/auth.json`, else `~/.codex/auth.json`, by default (openai/codex at f82e365,
//!   `codex-rs/login/src/auth/storage.rs:47-73,162-163` and
//!   `codex-rs/utils/home-dir/src/lib.rs:5-18`). Its `keyring` and `auto` stores keep it in the
//!   system keyring instead (the service `"Codex Auth"`, `storage.rs:243`), which tcr never
//!   opens. A metadata check proves only that the file exists, not whether a keyring-backed
//!   login is present. Doctor reports that case as unchecked; a file tcr cannot read still
//!   counts as a file, without tcr holding a copy of someone else's token.
//! - **The binary** ([`find_codex`]). One rule finds `codex` for `tcr login --codex`,
//!   `tcr run --codex` and the doctor, so the three never disagree about which codex a person
//!   has. A set `TCR_CODEX_BIN` or `CODEX_BIN` always decides and never falls through, so a
//!   typo in it is reported rather than silently replaced by whatever `codex` is on `PATH`.
//! - **Older `tcr`s** ([`older_tcrs`]). Each candidate runs `--version` and `login --help`,
//!   never anything that reads or writes a config.
//!
//! # Why the search looks past `PATH`
//!
//! A TcrBar-launched child inherits launchd's `PATH`, `/usr/bin:/bin:/usr/sbin:/sbin`
//! (`apps/macos/Sources/TcrBarCore/TcrTool.swift:39-40`, the doc of
//! `TcrTool.fallbackDirectories`), so `PATH` alone never reaches the `codex` a person installed,
//! nor the CLI `tcr` that `scripts/install-cli.sh` puts in `~/.local/bin`. The search therefore
//! adds TcrBar's own directories (`~/.local/bin`, `~/.cargo/bin`, `/opt/homebrew/bin`,
//! `/usr/local/bin`) and, for `codex`, npm's global prefix (`NPM_CONFIG_PREFIX`, else
//! `npm_config_prefix`, else `~/.npm-global`; tcr never runs npm).
//!
//! # The two test seams
//!
//! [`SEARCH_DIRS_ENV`] replaces the four directories and the npm prefix, and [`BUNDLED_TCR_ENV`]
//! replaces TcrBar's bundled `tcr`. The production binary reads them because the integration
//! tests run that binary: without them a test could not keep a real `codex` or TcrBar's real
//! `tcr` out of reach, since `/opt/homebrew/bin` and `/Applications` are not under a scratch
//! `HOME`. An empty value counts as unset, as for every tcr override.

use std::io::Read as _;

use crate::log_err::LogErr as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Names the `codex` binary to use, ahead of everything else.
pub const TCR_CODEX_BIN_ENV: &str = "TCR_CODEX_BIN";
/// Names the `codex` binary when [`TCR_CODEX_BIN_ENV`] is unset.
pub const CODEX_BIN_ENV: &str = "CODEX_BIN";
/// For tests: a colon-separated list that replaces TcrBar's four directories and npm's prefix,
/// in the codex search and in the older-tcr scan alike.
pub const SEARCH_DIRS_ENV: &str = "TCR_CODEX_SEARCH_DIRS";
/// For tests: the path that replaces TcrBar's bundled `tcr` in the older-tcr scan. A missing
/// path means no bundled `tcr`.
pub const BUNDLED_TCR_ENV: &str = "TCR_BUNDLED_TCR";
/// Where TcrBar keeps the `tcr` it bundles and runs ahead of `PATH`.
pub const BUNDLED_TCR: &str = "/Applications/TcrBar.app/Contents/MacOS/tcr";
/// The two system directories a Homebrew or hand-installed `codex` or `node` usually sits in.
const SYSTEM_DIRS: [&str; 2] = ["/opt/homebrew/bin", "/usr/local/bin"];
/// How long one `--version` or `login --help` of a candidate `tcr` may run.
const OLDER_TCR_DEADLINE: Duration = Duration::from_secs(5);

/// The `codex` binary [`find_codex`] settled on, and which rule found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexBin {
    /// The path as found, never canonicalised: an npm `bin/codex` is a symlink, and the
    /// directory it sits in is where its `node` is.
    pub path: PathBuf,
    pub source: CodexBinSource,
}

/// Which rule of [`find_codex`] found the binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexBinSource {
    TcrCodexBin,
    CodexBin,
    Path,
    SearchDirs,
    NpmPrefix,
}

impl CodexBinSource {
    /// How the doctor names the rule.
    pub const fn label(self) -> &'static str {
        match self {
            Self::TcrCodexBin => TCR_CODEX_BIN_ENV,
            Self::CodexBin => CODEX_BIN_ENV,
            Self::Path => "PATH",
            Self::SearchDirs => "TcrBar's directories",
            Self::NpmPrefix => "npm's global prefix",
        }
    }
}

/// Why [`find_codex`] found no `codex`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexSearch {
    /// Nothing anywhere: every place searched, in order.
    NotFound { searched: Vec<String> },
    /// A set variable names no executable. `bare` is a value with no `/`, looked up on `PATH`.
    BadVariable {
        var: &'static str,
        value: String,
        bare: bool,
    },
}

impl CodexSearch {
    /// Every place searched, joined with `, `; empty for a variable that decided.
    pub fn list(&self) -> String {
        match self {
            Self::NotFound { searched } => searched.join(", "),
            Self::BadVariable { .. } => String::new(),
        }
    }

    /// What went wrong and what to do, without saying whether anything was written: `tcr run
    /// --codex` writes nothing in any case, and a login adds that it wrote nothing.
    pub fn sentence(&self) -> String {
        match self {
            Self::NotFound { .. } => format!(
                "could not find the codex CLI (searched {}). Install it, or set TCR_CODEX_BIN to \
                 its path.",
                self.list()
            ),
            Self::BadVariable {
                var,
                value,
                bare: false,
            } => format!(
                "{var} is {value}, which is not an executable file. Point it at the codex \
                 executable, or unset it."
            ),
            Self::BadVariable {
                var,
                value,
                bare: true,
            } => format!(
                "{var} is {value}, and no executable {value} is on PATH. Point it at the codex \
                 executable, or unset it."
            ),
        }
    }

    /// [`Self::sentence`], as a login's refusal, which ends saying nothing was written.
    pub fn refusal(&self) -> String {
        format!("{} Nothing was written.", self.sentence())
    }
}

/// The `codex` this process should run, by the one rule in the module doc, reading this
/// process's environment.
pub fn find_codex() -> Result<CodexBin, CodexSearch> {
    find_codex_from(&env_value, &home_dir(), &is_executable_file)
}

/// [`find_codex`] over values already read: `env` answers a variable's value, `home` is the
/// user's home, and `is_executable` decides what counts as an executable file. It takes no
/// process runner: finding `codex` never runs anything.
pub fn find_codex_from(
    env: &dyn Fn(&str) -> Option<String>,
    home: &Path,
    is_executable: &dyn Fn(&Path) -> bool,
) -> Result<CodexBin, CodexSearch> {
    let set = |key: &str| env(key).filter(|value| !value.is_empty());
    let path_dirs = split_dirs(set("PATH").as_deref());
    for (var, source) in [
        (TCR_CODEX_BIN_ENV, CodexBinSource::TcrCodexBin),
        (CODEX_BIN_ENV, CodexBinSource::CodexBin),
    ] {
        let Some(value) = set(var) else { continue };
        let bare = !value.contains('/');
        let found = if bare {
            path_dirs
                .iter()
                .map(|dir| dir.join(&value))
                .find(|candidate| is_executable(candidate))
        } else {
            Some(PathBuf::from(&value)).filter(|candidate| is_executable(candidate))
        };
        return match found {
            Some(path) => Ok(CodexBin { path, source }),
            None => Err(CodexSearch::BadVariable { var, value, bare }),
        };
    }
    for dir in &path_dirs {
        let candidate = dir.join("codex");
        if is_executable(&candidate) {
            return Ok(CodexBin {
                path: candidate,
                source: CodexBinSource::Path,
            });
        }
    }
    let extra = codex_search_dirs(env, home);
    for (dir, source) in &extra {
        let candidate = dir.join("codex");
        if is_executable(&candidate) {
            return Ok(CodexBin {
                path: candidate,
                source: *source,
            });
        }
    }
    let mut searched: Vec<String> = [TCR_CODEX_BIN_ENV, CODEX_BIN_ENV, "PATH"]
        .iter()
        .map(|name| (*name).to_string())
        .collect();
    searched.extend(extra.iter().map(|(dir, _)| dir.display().to_string()));
    Err(CodexSearch::NotFound { searched })
}

/// Where the search goes after `PATH`: the [`SEARCH_DIRS_ENV`] list when set, else TcrBar's
/// four directories and then npm's global prefix's `bin`.
fn codex_search_dirs(
    env: &dyn Fn(&str) -> Option<String>,
    home: &Path,
) -> Vec<(PathBuf, CodexBinSource)> {
    if let Some(seam) = env(SEARCH_DIRS_ENV).filter(|value| !value.is_empty()) {
        return split_dirs(Some(&seam))
            .into_iter()
            .map(|dir| (dir, CodexBinSource::SearchDirs))
            .collect();
    }
    let mut dirs: Vec<(PathBuf, CodexBinSource)> = tcrbar_dirs(home)
        .into_iter()
        .map(|dir| (dir, CodexBinSource::SearchDirs))
        .collect();
    let prefix = env("NPM_CONFIG_PREFIX")
        .filter(|value| !value.is_empty())
        .or_else(|| env("npm_config_prefix").filter(|value| !value.is_empty()))
        .map_or_else(|| home.join(".npm-global"), PathBuf::from);
    dirs.push((prefix.join("bin"), CodexBinSource::NpmPrefix));
    dirs
}

/// TcrBar's own search directories, in its order.
fn tcrbar_dirs(home: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![home.join(".local/bin"), home.join(".cargo/bin")];
    dirs.extend(SYSTEM_DIRS.iter().map(PathBuf::from));
    dirs
}

/// The directories the older-tcr scan looks in beside `PATH`: the [`SEARCH_DIRS_ENV`] list when
/// set, else TcrBar's four directories. Never npm's prefix, so no `npm` is ever consulted for it.
fn older_tcr_dirs(env: &dyn Fn(&str) -> Option<String>, home: &Path) -> Vec<PathBuf> {
    match env(SEARCH_DIRS_ENV).filter(|value| !value.is_empty()) {
        Some(seam) => split_dirs(Some(&seam)),
        None => tcrbar_dirs(home),
    }
}

/// A colon-separated list of directories, empty entries dropped.
fn split_dirs(list: Option<&str>) -> Vec<PathBuf> {
    list.map(|list| {
        list.split(':')
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from)
            .collect()
    })
    .unwrap_or_default()
}

/// An executable regular file, after following symlinks (Homebrew's `bin/codex` is one): a
/// `metadata` call, `is_file()`, and any execute bit.
pub fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

fn env_value(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from)
}

/// Where codex keeps its login on this machine, by codex's own rule: `$CODEX_HOME/auth.json`,
/// else `~/.codex/auth.json`.
pub fn codex_login_path() -> PathBuf {
    codex_login_path_from(
        std::env::var_os(crate::codex_login::CODEX_HOME_ENV)
            .as_deref()
            .map(Path::new),
        &home_dir(),
    )
}

/// [`codex_login_path`] over values already read. An empty `CODEX_HOME` is no `CODEX_HOME`.
pub fn codex_login_path_from(codex_home: Option<&Path>, home: &Path) -> PathBuf {
    crate::paths::codex_auth_file(&crate::paths::codex_home_in(
        codex_home
            .filter(|dir| !dir.as_os_str().is_empty())
            .map(Path::to_path_buf),
        Some(home.to_path_buf()),
    ))
}

/// Whether codex has a login file of its own ([`login_file_exists`] at [`codex_login_path`]).
pub fn codex_login_exists() -> bool {
    login_file_exists(&codex_login_path())
}

/// Whether a login file is at `path`. Existence only: the file is never opened, so one tcr
/// cannot read still counts.
pub fn login_file_exists(path: &Path) -> bool {
    std::fs::metadata(path).is_ok()
}

/// The `PATH` that `tcr login --codex` runs `codex login` with.
///
/// An npm, nvm or asdf `codex` is a `#!/usr/bin/env node` launcher, and a TcrBar started from
/// Finder passes `/usr/bin:/bin:/usr/sbin:/sbin`, where `env` finds no `node` and exits 127. So
/// first comes the directory holding `codex` as found, made absolute but never canonicalised
/// (an npm `bin/codex` is a symlink into `lib/node_modules`, and `node` sits beside the link, not
/// beside its target); then the `inherited` `PATH`; then whichever of `fixed` it still lacks
/// (`/opt/homebrew/bin` and `/usr/local/bin`, or the [`SEARCH_DIRS_ENV`] list). A directory
/// already on the `PATH` is not added again. `tcr run --codex` does not use this: its `PATH` is
/// the one every command the agent runs inherits, so tcr must not reorder it.
pub fn codex_child_path(codex: &Path, inherited: Option<&str>, fixed: &[PathBuf]) -> String {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let inherited_dirs = split_dirs(inherited);
    let own = std::path::absolute(codex)
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf));
    if let Some(own) = own
        && !inherited_dirs.contains(&own)
    {
        dirs.push(own);
    }
    dirs.extend(inherited_dirs.iter().cloned());
    for dir in fixed {
        if !dirs.contains(dir) {
            dirs.push(dir.clone());
        }
    }
    dirs.iter()
        .map(|dir| dir.display().to_string())
        .collect::<Vec<_>>()
        .join(":")
}

/// The `PATH` to run `codex` at `codex` with, from this process's environment
/// ([`codex_child_path`]).
pub fn codex_login_child_path(codex: &Path) -> String {
    let fixed = match env_value(SEARCH_DIRS_ENV).filter(|value| !value.is_empty()) {
        Some(seam) => split_dirs(Some(&seam)),
        None => SYSTEM_DIRS.iter().map(PathBuf::from).collect(),
    };
    codex_child_path(codex, env_value("PATH").as_deref(), &fixed)
}

/// A notice a Codex save prints: the sentence, and whether it is a warning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexNotice {
    pub text: String,
    pub warning: bool,
}

/// The notices a Codex save prints, in order, each only when it holds: the first-account notice,
/// `first_account`, when the config held no Codex row in any state before the save
/// ([`crate::oauth::first_codex_account_notice`] words it); the no-login notice when codex has no
/// login file of its own; then one warning per older `tcr`.
pub fn codex_notices(first_account: Option<String>) -> Vec<CodexNotice> {
    let mut notices = Vec::new();
    if let Some(text) = first_account {
        notices.push(CodexNotice {
            text,
            warning: false,
        });
    }
    if !codex_login_exists() {
        notices.push(CodexNotice {
            text: crate::oauth::codex_has_no_login_notice(&codex_login_path()),
            warning: false,
        });
    }
    notices.extend(older_tcrs().iter().map(|older| CodexNotice {
        text: older.notice(),
        warning: true,
    }));
    notices
}

/// How an older `tcr` misreads a config holding a Codex row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OlderTcrKind {
    /// 0.2.45 or older: it predates the `credentialVersion` check, so it reads the row as a
    /// Claude account and sends its token to Anthropic.
    ReadsCodexAsClaude,
    /// From 0.2.46, still without `--from-codex`: it refuses the whole config.
    RefusesConfig,
}

/// A `tcr` on this machine that predates Codex accounts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OlderTcr {
    /// The path it was found at, as found.
    pub path: PathBuf,
    /// The version it reported.
    pub version: String,
    pub kind: OlderTcrKind,
}

impl OlderTcr {
    /// The warning this `tcr` gets, worded by what it would do.
    pub fn notice(&self) -> String {
        match self.kind {
            OlderTcrKind::ReadsCodexAsClaude => {
                crate::oauth::older_tcr_reads_codex_as_claude_notice(&self.path, &self.version)
            }
            OlderTcrKind::RefusesConfig => {
                crate::oauth::older_tcr_refuses_config_notice(&self.path, &self.version)
            }
        }
    }
}

/// Every `tcr` this machine can start that predates Codex accounts: a `tcr` in each absolute
/// `PATH` entry, then in each directory of the search (TcrBar's four, or the
/// [`SEARCH_DIRS_ENV`] list), then TcrBar's bundled one ([`BUNDLED_TCR_ENV`] overrides where).
/// This binary is never one of them.
///
/// A relative `PATH` entry (`.`, or `bin`) names a directory under wherever the login was
/// started, which can be a repository someone else wrote, and the scan runs every `tcr` it
/// lists, where the shell would run only the first. So a relative entry is skipped.
pub fn older_tcrs() -> Vec<OlderTcr> {
    let home = home_dir();
    let mut candidates: Vec<PathBuf> = split_dirs(env_value("PATH").as_deref())
        .into_iter()
        .filter(|dir| dir.is_absolute())
        .chain(older_tcr_dirs(&env_value, &home))
        .map(|dir| dir.join("tcr"))
        .collect();
    candidates.push(
        env_value(BUNDLED_TCR_ENV)
            .filter(|value| !value.is_empty())
            .map_or_else(|| PathBuf::from(BUNDLED_TCR), PathBuf::from),
    );
    candidates.retain(|candidate| is_executable_file(candidate));
    older_tcrs_from(candidates, std::env::current_exe().ok(), &|path, args| {
        run_with_deadline(path, args, OLDER_TCR_DEADLINE)
    })
}

/// [`older_tcrs`] over candidates already listed: each is asked `--version` and `login --help`
/// through `run`, which answers a command's stdout, or `None` when it failed or overran.
/// Candidates are de-duplicated by canonical path, and `this_exe` (canonicalised too, so a
/// symlink to this binary is skipped as well) is never asked anything. A version that does not
/// parse gives no notice, and neither does a help that could not be read: a warning that might
/// be false is worse than none.
pub fn older_tcrs_from(
    candidates: Vec<PathBuf>,
    this_exe: Option<PathBuf>,
    run: &dyn Fn(&Path, &[&str]) -> Option<String>,
) -> Vec<OlderTcr> {
    let canonical =
        |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let this_exe = this_exe.as_deref().map(canonical);
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut older = Vec::new();
    for path in candidates {
        let real = canonical(&path);
        if Some(&real) == this_exe.as_ref() || seen.contains(&real) {
            continue;
        }
        seen.push(real);
        let Some(version) = run(&path, &["--version"])
            .as_deref()
            .and_then(parse_version)
        else {
            continue;
        };
        let (major, minor, patch) = version.1;
        let kind = if (major, minor, patch) <= (0, 2, 45) {
            OlderTcrKind::ReadsCodexAsClaude
        } else {
            match run(&path, &["login", "--help"]) {
                Some(help) if !help_has_whole_token(&help, "--from-codex") => {
                    OlderTcrKind::RefusesConfig
                }
                _ => continue,
            }
        };
        older.push(OlderTcr {
            path,
            version: version.0,
            kind,
        });
    }
    older
}

/// The first `major.minor.patch` token of a `--version` answer (`tcr 0.2.44`), as written and as
/// numbers. A pre-release or build suffix after the patch number is kept in the text and ignored
/// in the numbers.
fn parse_version(answer: &str) -> Option<(String, (u64, u64, u64))> {
    answer.split_whitespace().find_map(|token| {
        let token = token.trim_start_matches('v');
        let mut parts = token.splitn(3, '.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let rest = parts.next()?;
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        let patch = digits.parse().ok()?;
        Some((token.to_string(), (major, minor, patch)))
    })
}

/// Whether `help` carries `token` as a whole token, splitting on whitespace, commas, `[`, `]`,
/// `(`, `)` and `=`: `--from-codex-home` is not `--from-codex`.
pub fn help_has_whole_token(help: &str, token: &str) -> bool {
    help.split(|c: char| c.is_whitespace() || matches!(c, ',' | '[' | ']' | '(' | ')' | '='))
        .any(|part| part == token)
}

/// Run `path args…` with stdin closed and stderr discarded, and answer its stdout when it exits
/// 0 and its stdout closes, both within `deadline`. One that overruns is killed and answers
/// `None`, as does one that cannot be started. One that exits in time while a process it started
/// still holds its stdout also answers `None` at the deadline: the thread reading the pipe is
/// left to finish on its own.
fn run_with_deadline(path: &Path, args: &[&str], deadline: Duration) -> Option<String> {
    let mut child = std::process::Command::new(path)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        stdout
            .read_to_string(&mut text)
            .log_warn("reading the probed command's output");
        text
    });
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                while !reader.is_finished() {
                    if started.elapsed() >= deadline {
                        return None;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                let text = reader.join().ok()?;
                return status.success().then_some(text);
            }
            Ok(None) if started.elapsed() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                child.kill().log_debug("killing a probe past its deadline");
                child.wait().log_debug("reaping a probe past its deadline");
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use std::os::unix::fs::PermissionsExt as _;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |key| map.get(key).cloned()
    }

    fn present(paths: &[&str]) -> impl Fn(&Path) -> bool + use<> {
        let set: HashSet<PathBuf> = paths.iter().map(PathBuf::from).collect();
        move |path| set.contains(path)
    }

    fn found(path: &str, source: CodexBinSource) -> Result<CodexBin, CodexSearch> {
        Ok(CodexBin {
            path: PathBuf::from(path),
            source,
        })
    }

    const HOME: &str = "/Users/example";

    /// The order, one place at a time: each case holds a `codex` in that place and in every
    /// later one, so the place that wins is the first in order.
    ///
    /// Watched red: with `/opt/homebrew/bin` searched before `~/.cargo/bin`, the cargo case
    /// finds the Homebrew codex.
    #[test]
    fn codex_is_resolved_in_order() {
        let home = Path::new(HOME);
        let everywhere = [
            "/x/tcr-codex",
            "/x/codex-bin",
            "/p/codex",
            "/Users/example/.local/bin/codex",
            "/Users/example/.cargo/bin/codex",
            "/opt/homebrew/bin/codex",
            "/usr/local/bin/codex",
            "/Users/example/.npm-global/bin/codex",
        ];
        let vars = [
            ("TCR_CODEX_BIN", "/x/tcr-codex"),
            ("CODEX_BIN", "/x/codex-bin"),
            ("PATH", "/p"),
        ];
        let wanted = [
            ("/x/tcr-codex", CodexBinSource::TcrCodexBin),
            ("/x/codex-bin", CodexBinSource::CodexBin),
            ("/p/codex", CodexBinSource::Path),
            (
                "/Users/example/.local/bin/codex",
                CodexBinSource::SearchDirs,
            ),
            (
                "/Users/example/.cargo/bin/codex",
                CodexBinSource::SearchDirs,
            ),
            ("/opt/homebrew/bin/codex", CodexBinSource::SearchDirs),
            ("/usr/local/bin/codex", CodexBinSource::SearchDirs),
            (
                "/Users/example/.npm-global/bin/codex",
                CodexBinSource::NpmPrefix,
            ),
        ];
        for (index, (path, source)) in wanted.iter().enumerate() {
            let vars_left: Vec<(&str, &str)> = vars.iter().skip(index.min(3)).copied().collect();
            let files: Vec<&str> = everywhere.iter().skip(index).copied().collect();
            assert_eq!(
                find_codex_from(&env_of(&vars_left), home, &present(&files)),
                found(path, *source),
                "case {index}"
            );
        }

        let none = find_codex_from(&env_of(&[]), home, &present(&[])).unwrap_err();
        assert_eq!(
            none.list(),
            "TCR_CODEX_BIN, CODEX_BIN, PATH, /Users/example/.local/bin, \
             /Users/example/.cargo/bin, /opt/homebrew/bin, /usr/local/bin, \
             /Users/example/.npm-global/bin"
        );
        assert_eq!(
            none.refusal(),
            "could not find the codex CLI (searched TCR_CODEX_BIN, CODEX_BIN, PATH, \
             /Users/example/.local/bin, /Users/example/.cargo/bin, /opt/homebrew/bin, \
             /usr/local/bin, /Users/example/.npm-global/bin). Install it, or set TCR_CODEX_BIN \
             to its path. Nothing was written."
        );
        for prefix in [
            vec![("NPM_CONFIG_PREFIX", "/opt/npm")],
            vec![("npm_config_prefix", "/opt/npm")],
        ] {
            let none = find_codex_from(&env_of(&prefix), home, &present(&[])).unwrap_err();
            assert!(
                none.list().ends_with(", /usr/local/bin, /opt/npm/bin"),
                "{none:?}"
            );
        }
        assert_eq!(
            find_codex_from(
                &env_of(&[("NPM_CONFIG_PREFIX", "/opt/npm")]),
                home,
                &present(&["/opt/npm/bin/codex"])
            ),
            found("/opt/npm/bin/codex", CodexBinSource::NpmPrefix)
        );
        let seam = env_of(&[("TCR_CODEX_SEARCH_DIRS", "/s1:/s2")]);
        assert_eq!(
            find_codex_from(&seam, home, &present(&[]))
                .unwrap_err()
                .list(),
            "TCR_CODEX_BIN, CODEX_BIN, PATH, /s1, /s2",
            "the seam replaces the four directories and the npm prefix"
        );
        assert_eq!(
            find_codex_from(
                &seam,
                home,
                &present(&["/s2/codex", "/opt/homebrew/bin/codex"])
            ),
            found("/s2/codex", CodexBinSource::SearchDirs)
        );
    }

    /// A set variable always decides, and a value that names no executable is refused, never
    /// replaced by a `codex` found elsewhere. Checked against the real filesystem, with the
    /// production predicate.
    ///
    /// Watched red: letting a bad variable fall through to the search finds the `codex` on
    /// `PATH`.
    #[test]
    fn a_set_codex_variable_always_decides() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let executable = |path: &Path| {
            std::fs::write(path, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        executable(&bin.join("codex-beta"));
        executable(&bin.join("codex"));
        let path_var = bin.display().to_string();
        let home = dir.path();

        assert_eq!(
            find_codex_from(
                &env_of(&[("TCR_CODEX_BIN", "codex-beta"), ("PATH", &path_var)]),
                home,
                &is_executable_file
            ),
            Ok(CodexBin {
                path: bin.join("codex-beta"),
                source: CodexBinSource::TcrCodexBin
            })
        );
        let missing_bare = find_codex_from(
            &env_of(&[("TCR_CODEX_BIN", "codex-gamma"), ("PATH", &path_var)]),
            home,
            &is_executable_file,
        )
        .unwrap_err();
        assert_eq!(
            missing_bare.refusal(),
            "TCR_CODEX_BIN is codex-gamma, and no executable codex-gamma is on PATH. Point it at \
             the codex executable, or unset it. Nothing was written."
        );

        let plain = dir.path().join("plain");
        std::fs::write(&plain, "not a program").unwrap();
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).unwrap();
        let app = dir.path().join("Codex.app");
        std::fs::create_dir(&app).unwrap();
        std::fs::set_permissions(&app, std::fs::Permissions::from_mode(0o755)).unwrap();
        let gone = dir.path().join("gone");
        for bad in [&plain, &app, &gone] {
            let value = bad.display().to_string();
            let refused = find_codex_from(
                &env_of(&[("TCR_CODEX_BIN", &value), ("PATH", &path_var)]),
                home,
                &is_executable_file,
            )
            .unwrap_err();
            assert_eq!(
                refused.refusal(),
                format!(
                    "TCR_CODEX_BIN is {value}, which is not an executable file. Point it at the \
                     codex executable, or unset it. Nothing was written."
                ),
                "the codex on PATH is never used instead"
            );
        }

        let link = dir.path().join("codex-link");
        std::os::unix::fs::symlink(bin.join("codex"), &link).unwrap();
        let value = link.display().to_string();
        assert_eq!(
            find_codex_from(&env_of(&[("CODEX_BIN", &value)]), home, &is_executable_file),
            Ok(CodexBin {
                path: link.clone(),
                source: CodexBinSource::CodexBin
            }),
            "a symlink to an executable is accepted, as found"
        );
        assert_eq!(
            find_codex_from(
                &env_of(&[("TCR_CODEX_BIN", ""), ("CODEX_BIN", &value)]),
                home,
                &is_executable_file
            ),
            Ok(CodexBin {
                path: link,
                source: CodexBinSource::CodexBin
            }),
            "an empty TCR_CODEX_BIN counts as unset"
        );
    }

    /// `codex login` runs with node beside its launcher: the launcher's own directory first, as
    /// found and not canonicalised, then the inherited `PATH`, then the fixed directories it
    /// lacks.
    ///
    /// Watched red: canonicalising the codex path puts the link's target directory first.
    #[test]
    fn codex_runs_with_node_beside_it() {
        let fixed: Vec<PathBuf> = SYSTEM_DIRS.iter().map(PathBuf::from).collect();
        assert_eq!(
            codex_child_path(
                Path::new("/Users/example/.nvm/versions/node/v22.11.0/bin/codex"),
                Some("/usr/bin:/bin:/usr/sbin:/sbin"),
                &fixed
            ),
            "/Users/example/.nvm/versions/node/v22.11.0/bin:/usr/bin:/bin:/usr/sbin:/sbin:\
             /opt/homebrew/bin:/usr/local/bin"
        );
        let full = "/usr/local/bin:/usr/bin:/opt/homebrew/bin:/bin";
        assert_eq!(
            codex_child_path(Path::new("/opt/homebrew/bin/codex"), Some(full), &fixed),
            full,
            "nothing is added twice"
        );

        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("lib/node_modules/@openai/codex/bin");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("codex.js"), "").unwrap();
        let link_dir = dir.path().join("bin");
        std::fs::create_dir(&link_dir).unwrap();
        std::os::unix::fs::symlink(real.join("codex.js"), link_dir.join("codex")).unwrap();
        let child_path = codex_child_path(&link_dir.join("codex"), Some("/usr/bin"), &[]);
        assert_eq!(
            child_path,
            format!("{}:/usr/bin", link_dir.display()),
            "the link's directory, where node sits, never its target's"
        );
    }

    /// The login check is an existence check, so a file tcr cannot read still counts.
    #[test]
    fn the_login_check_is_existence_only() {
        let home = Path::new(HOME);
        assert_eq!(
            codex_login_path_from(Some(Path::new("/Users/example/elsewhere")), home),
            PathBuf::from("/Users/example/elsewhere/auth.json")
        );
        assert_eq!(
            codex_login_path_from(None, home),
            PathBuf::from("/Users/example/.codex/auth.json")
        );
        assert_eq!(
            codex_login_path_from(Some(Path::new("")), home),
            PathBuf::from("/Users/example/.codex/auth.json"),
            "an empty CODEX_HOME is no CODEX_HOME"
        );

        let dir = tempfile::tempdir().unwrap();
        let auth = dir.path().join("auth.json");
        assert!(!login_file_exists(&auth), "missing: no login");
        std::fs::write(&auth, "{}").unwrap();
        std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o000)).unwrap();
        assert!(
            std::fs::read(&auth).is_err() || running_as_root(),
            "a read of this file fails"
        );
        assert!(login_file_exists(&auth), "and it still counts as present");
        std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    /// Root reads a 0o000 file anyway; the existence assertion is the one that matters.
    fn running_as_root() -> bool {
        std::env::var("USER").is_ok_and(|user| user == "root")
    }

    /// An answer per path and per command, for [`older_tcrs_from`].
    fn answers(
        table: &[(&str, &str, Option<&str>)],
    ) -> impl Fn(&Path, &[&str]) -> Option<String> + use<> {
        let table: Vec<(PathBuf, String, Option<String>)> = table
            .iter()
            .map(|(path, version, help)| {
                (
                    PathBuf::from(path),
                    (*version).to_string(),
                    help.map(str::to_string),
                )
            })
            .collect();
        move |path, args| {
            let (_, version, help) = table.iter().find(|(p, ..)| p == path)?;
            match args {
                ["--version"] => Some(version.clone()),
                ["login", "--help"] => help.clone(),
                _ => None,
            }
        }
    }

    const HELP_WITH: &str = "Options:\n      --from-codex  Import\n      --codex-home <DIR>\n";
    const HELP_WITHOUT: &str = "Options:\n      --from-claude-code  Import\n";
    const HELP_LOOKALIKE: &str = "Options:\n      --from-codex-home <DIR>  Import\n";

    /// Each older `tcr` gets the notice its version earns, and a current one gets none.
    ///
    /// Watched red: matching `--from-codex` as a substring gives the lookalike help no notice.
    #[test]
    fn an_older_tcr_gets_the_notice_for_its_version() {
        let run = answers(&[
            ("/a/tcr", "tcr 0.2.44\n", Some(HELP_WITHOUT)),
            ("/b/tcr", "tcr 0.2.45\n", Some(HELP_WITHOUT)),
            ("/c/tcr", "tcr 0.2.46\n", Some(HELP_WITHOUT)),
            ("/d/tcr", "tcr 1.1.18\n", Some(HELP_LOOKALIKE)),
            ("/e/tcr", "tcr 1.2.0\n", Some(HELP_WITH)),
            ("/f/tcr", "tcr nightly\n", Some(HELP_WITHOUT)),
        ]);
        let candidates = ["/a/tcr", "/b/tcr", "/c/tcr", "/d/tcr", "/e/tcr", "/f/tcr"]
            .iter()
            .map(PathBuf::from)
            .collect();
        let notices: Vec<String> = older_tcrs_from(candidates, None, &run)
            .iter()
            .map(OlderTcr::notice)
            .collect();
        assert_eq!(
            notices,
            [
                "the tcr at /a/tcr (0.2.44) predates Codex accounts: update it before it next \
                 starts — it would read this login as a Claude account and send its token to \
                 Anthropic",
                "the tcr at /b/tcr (0.2.45) predates Codex accounts: update it before it next \
                 starts — it would read this login as a Claude account and send its token to \
                 Anthropic",
                "the tcr at /c/tcr (0.2.46) predates Codex accounts: update it — until then it \
                 refuses this config, so every command it runs fails",
                "the tcr at /d/tcr (1.1.18) predates Codex accounts: update it — until then it \
                 refuses this config, so every command it runs fails",
            ]
        );
        assert!(help_has_whole_token("[--from-codex]", "--from-codex"));
        assert!(help_has_whole_token("(--from-codex=x)", "--from-codex"));
        assert!(!help_has_whole_token("--from-codex-home", "--from-codex"));
    }

    /// This binary is never warned about: not by its own path, and not through a symlink to it.
    ///
    /// Watched red: without the skip, both answer as an old `tcr` and are flagged.
    #[test]
    fn this_binary_is_never_flagged() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("tcr");
        std::fs::write(&exe, "").unwrap();
        let other = dir.path().join("other");
        std::fs::create_dir(&other).unwrap();
        let link = other.join("tcr");
        std::os::unix::fs::symlink(&exe, &link).unwrap();
        let run = |_: &Path, args: &[&str]| match args {
            ["--version"] => Some("tcr 0.2.44".to_string()),
            _ => Some(HELP_WITHOUT.to_string()),
        };
        assert_eq!(
            older_tcrs_from(vec![exe.clone(), link], Some(exe), &run),
            Vec::new()
        );
    }

    /// The deadline bounds the whole answer, not only the exit. A `tcr` that exits 0 while a
    /// process it started still holds its stdout answers `None` at the deadline, instead of
    /// holding the login until that process lets go of the pipe. The script exits well inside
    /// the deadline (`sh` alone can take a few hundred milliseconds to start), and its
    /// background `sleep` outlives it by far.
    ///
    /// Watched red: waiting for the pipe to close, this answers the version once the `sleep`
    /// ends, 20 seconds in.
    #[test]
    fn a_tcr_that_leaves_its_output_open_answers_none_at_the_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("tcr");
        std::fs::write(&script, "#!/bin/sh\n/bin/sleep 20 &\necho tcr 0.2.44\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let started = Instant::now();
        let answer = run_with_deadline(&script, &["--version"], Duration::from_secs(3));
        let took = started.elapsed();
        assert_eq!(answer, None, "answered after {took:?}");
        assert!(took < Duration::from_secs(10), "answered after {took:?}");
    }
}
