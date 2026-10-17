//! The environment every test launch of the built `tcr` starts from.
//!
//! A child that inherits the developer's environment reads it: a real `HOME`, the login
//! Keychain's Claude Code credential, `SSL_CERT_FILE`, a proxy, a `ZDOTDIR` whose startup files
//! `tcr doctor` scans. [`tcr`] clears all of it and sets back only what a launch needs, so every
//! test starts from the same machine: a scratch `HOME`, a Claude Code login that does not exist,
//! a system `PATH`, and nothing else. A variable that is absent is the whole of "pointed
//! nowhere" for the proxy variables (`HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY`, `NO_PROXY` and
//! their lowercase forms), `SSL_CERT_FILE`, `CODEX_CA_CERTIFICATE`, `NODE_EXTRA_CA_CERTS`,
//! `XDG_CACHE_HOME`, `XDG_CONFIG_HOME`, `ZDOTDIR`, `CODEX_HOME`, `TCR_CODEX_AUTH` and
//! `ANTHROPIC_BASE_URL`: a launch that wants one sets it after calling [`tcr`], by name.
use std::path::Path;
use std::process::Command;

/// The directories a launch may find system tools in, and no developer-installed ones.
const SYSTEM_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

/// The built `tcr` with the baseline environment over `home`.
pub fn tcr(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tcr"));
    isolate(&mut command, home);
    command
}

/// The baseline environment over `home` on a command that is not the built binary itself (a
/// symlink to it, say). Call it before any `.env`: it clears whatever came before.
///
/// `TCR_CLAUDE_CODE_CREDENTIALS` names a file that does not exist, because a scratch `HOME` does
/// not scratch the login Keychain and the first run imports this machine's Claude Code login
/// when it finds one; an empty value reads as unset and falls through to the Keychain.
pub fn isolate<'a>(command: &'a mut Command, home: &Path) -> &'a mut Command {
    command
        .env_clear()
        .env("PATH", SYSTEM_PATH)
        .env("HOME", home)
        .env("CFFIXED_USER_HOME", home)
        .env(
            "TCR_CLAUDE_CODE_CREDENTIALS",
            home.join("no-claude-code-login.json"),
        )
}
