//! Keep the agents' CONNECT route out of their shell tools. Claude's shell prefix
//! covers shell hooks/MCP too; its env file runs after the Bash snapshot. Codex's
//! shell policy applies after its snapshot. Neither changes a model base URL.
//!
//! PATH launchers put ordinary nested `claude`/`codex` invocations back through
//! the pool. Absolute executable paths and explicit environment/config overrides
//! can bypass this convenience; this is routing, not a security boundary.
//!
//! Helpers are content-addressed, private cache files. Keeping them after the
//! foreground agent exits lets background shells finish and avoids per-launch
//! cache growth. They contain executable/CA paths and a loopback proxy URL, never
//! account credentials. No user shell or client configuration is edited.

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context;
use sha2::{Digest, Sha256};

const PROXIES: [&str; 6] = [
    "HTTPS_PROXY",
    "https_proxy",
    "HTTP_PROXY",
    "http_proxy",
    "ALL_PROXY",
    "all_proxy",
];

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn path_text(path: &Path) -> anyhow::Result<&str> {
    path.to_str()
        .context("agent helper paths must be valid UTF-8")
}

fn inherited(cmd: &Command, name: &str) -> Option<String> {
    match cmd.get_envs().find(|(key, _)| *key == name) {
        Some((_, value)) => value.map(|v| v.to_string_lossy().into_owned()),
        None => std::env::var(name).ok(),
    }
}

/// Resolve before putting the helpers on PATH, and retain symlinks (npm relies
/// on their parent directory). Absolute paths also survive a tool changing cwd.
fn executable(name: &Path) -> Option<PathBuf> {
    let path = if name.components().count() > 1 || name.is_absolute() {
        name.to_path_buf()
    } else {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|dir| dir.join(name))
            .find(|p| tcr::codex_local::is_executable_file(p))?
    };
    tcr::codex_local::is_executable_file(&path)
        .then(|| std::path::absolute(path).ok())
        .flatten()
}

fn publish(path: &Path, contents: &str) -> anyhow::Result<()> {
    if std::fs::read_to_string(path).ok().as_deref() == Some(contents) {
        return Ok(());
    }
    // Atomic replacement: another session may already be executing this helper.
    let mut file = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    file.as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o700))?;
    file.write_all(contents.as_bytes())?;
    file.persist(path)?;
    Ok(())
}

fn codex_policy() -> Vec<String> {
    // `set` composes with both legacy exclude/include_only and current filters.
    // Setting empty strings also wins over inherited ALL_PROXY fallbacks.
    PROXIES
        .iter()
        .map(|key| format!("shell_environment_policy.set.{key}=\"\""))
        .collect()
}

pub fn apply_codex_shell_policy(cmd: &mut Command) {
    for setting in codex_policy() {
        cmd.args(["-c", &setting]);
    }
    eprintln!(
        "[tcr] shell tools connect directly; nested codex on PATH uses the current pool checks"
    );
}

/// Apply only after see-through routing is selected, before user CLI arguments.
pub fn apply(cmd: &mut Command, claude: &Path, config: &Path) -> anyhow::Result<()> {
    let claude = executable(claude);
    let tcr = std::env::current_exe()?;
    let config = std::path::absolute(config)?;
    let codex_bin = tcr::codex_local::find_codex()
        .ok()
        .and_then(|found| executable(&found.path));
    let prefix = inherited(cmd, "CLAUDE_CODE_SHELL_PREFIX").filter(|v| !v.is_empty());
    let env_file = inherited(cmd, "CLAUDE_ENV_FILE").filter(|v| !v.is_empty());
    let shell = inherited(cmd, "CLAUDE_CODE_SHELL")
        .or_else(|| inherited(cmd, "SHELL"))
        .filter(|s| {
            matches!(
                Path::new(s).file_name().and_then(|v| v.to_str()),
                Some("bash" | "zsh")
            )
        })
        .unwrap_or_else(|| "/bin/bash".into());
    let mut agent_env = BTreeMap::new();
    for key in [
        "HTTPS_PROXY",
        "https_proxy",
        "NODE_EXTRA_CA_CERTS",
        "CODEX_CA_CERTIFICATE",
    ] {
        if let Some(value) = inherited(cmd, key) {
            agent_env.insert(key, value);
        }
    }
    let seed = serde_json::to_vec(&(
        "tool-env-v2",
        &tcr,
        &config,
        &claude,
        &codex_bin,
        &prefix,
        &env_file,
        &shell,
        &agent_env,
    ))?;
    let root = tcr::paths::cache_dir().root().join("tool-env").join(
        Sha256::digest(seed)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>(),
    );
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&root)?;
    let root_text = path_text(&root)?;
    let cleanup = format!("unset {}\n", PROXIES.join(" "));
    let prepend = format!("export PATH={}:\"$PATH\"\n", quote(root_text));
    let mut env_script = String::new();
    if let Some(file) = &env_file {
        env_script.push_str(&format!(". {}\n", quote(file)));
    }
    env_script.push_str(&cleanup);
    env_script.push_str(&prepend);
    publish(&root.join("shell-env"), &env_script)?;
    let exec = match prefix {
        Some(prefix) => format!("exec {} \"$@\"\n", quote(&prefix)),
        None => format!("exec {} -c \"$1\"\n", quote(&shell)),
    };
    publish(
        &root.join("shell-prefix"),
        &format!("#!/bin/sh\n{cleanup}{prepend}{exec}"),
    )?;

    let mut restore = String::new();
    for (key, value) in &agent_env {
        restore.push_str(&format!("export {key}={}\n", quote(value)));
    }
    restore.push_str(&format!(
        "export CLAUDE_CODE_SHELL_PREFIX={}\nexport CLAUDE_ENV_FILE={}\n",
        quote(path_text(&root.join("shell-prefix"))?),
        quote(path_text(&root.join("shell-env"))?),
    ));
    if let Some(bin) = claude {
        publish(&root.join("claude"), &format!(
            "#!/bin/sh\n{restore}export ANTHROPIC_BASE_URL=https://api.anthropic.com\nexec {} \"$@\"\n",
            quote(path_text(&bin)?),
        ))?;
    }
    if let Some(bin) = codex_bin {
        // Reuse the current launch decision, including live interception and
        // caller CA checks. Pin the real executable before adding our PATH shim.
        publish(
            &root.join("codex"),
            &format!(
                "#!/bin/sh\nexport TCR_CODEX_BIN={}\nexec {} run --codex --config {} -- \"$@\"\n",
                quote(path_text(&bin)?),
                quote(path_text(&tcr)?),
                quote(path_text(&config)?),
            ),
        )?;
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    let paths = std::iter::once(root.clone()).chain(std::env::split_paths(&path));
    cmd.env("PATH", std::env::join_paths(paths)?);
    cmd.env("CLAUDE_CODE_SHELL_PREFIX", root.join("shell-prefix"));
    cmd.env("CLAUDE_ENV_FILE", root.join("shell-env"));
    eprintln!("[tcr] shell tools connect directly; nested claude/codex on PATH use the pool");
    Ok(())
}
