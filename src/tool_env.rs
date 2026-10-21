//! Separate shell-tool routing from the model client's proxy. Private launchers
//! restore nested agents; their scripts contain no proxy values or request keys.
//! A launch owns its temporary directory and removes it when the client exits.
use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context;

const PROXIES: [&str; 6] = [
    "HTTPS_PROXY",
    "https_proxy",
    "HTTP_PROXY",
    "http_proxy",
    "ALL_PROXY",
    "all_proxy",
];
const CAS: [&str; 2] = ["NODE_EXTRA_CA_CERTS", "CODEX_CA_CERTIFICATE"];

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn path_text(path: &Path) -> anyhow::Result<&str> {
    path.to_str()
        .context("agent helper paths must be valid UTF-8")
}

/// What a launch reads from the machine it runs on: the environment it inherits
/// (which also names `HOME` and `CODEX_HOME`, so the Codex config file to read)
/// and tcr's own CA path. Production uses [`Host::current`]; a test passes its
/// own, so a test run inside a `tcr run` session never sees that session's proxy.
pub(crate) struct Host<'a> {
    pub env: &'a dyn Fn(&str) -> Option<String>,
    pub own_ca: PathBuf,
}

fn process_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

impl Host<'static> {
    /// This process's environment and tcr's minted CA path.
    pub(crate) fn current() -> Self {
        Self {
            env: &process_env,
            own_ca: tcr::mitm::minted_ca_path(),
        }
    }
}

fn inherited(cmd: &Command, name: &str, host: &Host<'_>) -> Option<String> {
    match cmd.get_envs().find(|(key, _)| *key == name) {
        Some((_, value)) => value.map(|v| v.to_string_lossy().into_owned()),
        None => (host.env)(name),
    }
}

/// Resolve PATH without re-entering an outer session's tcr launchers. Retain
/// npm symlinks: their parent directory is meaningful to the executable.
pub(crate) fn executable(name: &Path) -> Option<PathBuf> {
    let path = if name.components().count() > 1 || name.is_absolute() {
        name.to_path_buf()
    } else {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|dir| dir.join(name))
            .find(|p| {
                tcr::codex_local::is_executable_file(p) && !tcr::codex_local::is_tool_launcher(p)
            })?
    };
    tcr::codex_local::is_executable_file(&path)
        .then(|| std::path::absolute(path).ok())
        .flatten()
}

fn publish(path: &Path, contents: &str) -> anyhow::Result<()> {
    let parent = path.parent().context("helper must have a parent")?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o700))?;
    file.write_all(contents.as_bytes())?;
    file.persist(path)?;
    Ok(())
}

fn model_port(cmd: &Command, host: &Host<'_>) -> Option<u16> {
    reqwest::Url::parse(&inherited(cmd, "HTTPS_PROXY", host)?)
        .ok()?
        .port_or_known_default()
}

fn caller_env(port: Option<u16>, host: &Host<'_>) -> BTreeMap<String, String> {
    PROXIES
        .iter()
        .chain(CAS.iter())
        .filter_map(|key| {
            let value = (host.env)(key)?;
            // Strip only our own CA; caller bundles remain part of the shell env.
            if (CAS.contains(key) && Path::new(&value) == host.own_ca)
                || (PROXIES.contains(key)
                    && port.is_some_and(|port| tcr::doctor::points_at_tcr(&value, port)))
            {
                return None;
            }
            Some(((*key).to_string(), value))
        })
        .collect()
}

/// Preserve explicit config-file set entries. CLI arguments appended by the
/// caller can override these defaults again. This reads, never edits, Codex's file.
fn codex_policy(cmd: &Command, host: &Host<'_>) -> BTreeMap<String, String> {
    let mut values = caller_env(model_port(cmd, host), host);
    let nonempty = |name| (host.env)(name).filter(|v| !v.is_empty());
    let file = nonempty("CODEX_HOME")
        .map(|home| PathBuf::from(home).join("config.toml"))
        .or_else(|| nonempty("HOME").map(|home| PathBuf::from(home).join(".codex/config.toml")));
    if let Some(file) = file
        && let Ok(text) = std::fs::read_to_string(file)
        && let Ok(document) = text.parse::<toml_edit::DocumentMut>()
        && let Some(set) = document
            .get("shell_environment_policy")
            .and_then(|p| p.get("set"))
            .and_then(toml_edit::Item::as_table_like)
    {
        for (key, item) in set.iter() {
            if let Some(value) = item.as_str() {
                values.insert(key.to_string(), value.to_string());
            }
        }
    }
    for key in PROXIES.iter().chain(CAS.iter()) {
        values.entry((*key).to_string()).or_default();
    }
    values
}

pub(crate) fn apply_codex_shell_policy(cmd: &mut Command, host: &Host<'_>) -> Vec<String> {
    let values = codex_policy(cmd, host);
    for key in PROXIES.iter().chain(CAS.iter()) {
        let value = values.get(*key).cloned().unwrap_or_default();
        cmd.args([
            "-c",
            &format!(
                "shell_environment_policy.set.{key}={}",
                serde_json::Value::String(value)
            ),
        ]);
    }
    vec!["[tcr] shell tools use the caller's proxy and CA settings; nested codex on PATH uses the current pool checks".into()]
}

/// Apply after model routing, before explicit CLI arguments. The returned owner
/// must live until the foreground agent exits, including error paths.
pub(crate) fn apply(
    cmd: &mut Command,
    claude: &Path,
    config: &Path,
    claude_shell: bool,
) -> anyhow::Result<(tempfile::TempDir, Vec<String>)> {
    let claude = executable(claude);
    let tcr = std::env::current_exe()?.canonicalize()?;
    let config = std::path::absolute(config)?;
    let codex_bin = tcr::codex_local::find_codex()
        .ok()
        .and_then(|found| executable(&found.path));
    let host = Host::current();
    let helper_parent = std::path::absolute(tcr::paths::cache_dir().root().join("tool-env"))?;
    let original = |key: &str, saved: &str| {
        inherited(cmd, key, &host)
            .filter(|v| !v.is_empty())
            .and_then(|value| {
                if tcr::codex_local::is_tool_launcher(Path::new(&value))
                    || Path::new(&value).starts_with(&helper_parent)
                {
                    (host.env)(saved).filter(|v| !v.is_empty())
                } else {
                    Some(value)
                }
            })
    };
    let prefix = original("CLAUDE_CODE_SHELL_PREFIX", "TCR_TOOL_ORIGINAL_PREFIX");
    // Claude splits a prefix at the last " -". Treating the whole string as an
    // executable breaks every shell command. Refuse unsupported flag syntax
    // before starting the agent rather than silently changing its meaning.
    anyhow::ensure!(
        !claude_shell
            || !prefix
                .as_ref()
                .is_some_and(|p| p.contains(" -") && !Path::new(p).is_file()),
        "CLAUDE_CODE_SHELL_PREFIX with flags is not supported by tcr's shell routing; use an executable wrapper script instead"
    );
    let env_file = original("CLAUDE_ENV_FILE", "TCR_TOOL_ORIGINAL_ENV_FILE");
    cmd.env(
        "TCR_TOOL_ORIGINAL_PREFIX",
        prefix.as_deref().unwrap_or_default(),
    );
    cmd.env(
        "TCR_TOOL_ORIGINAL_ENV_FILE",
        env_file.as_deref().unwrap_or_default(),
    );
    let shell = inherited(cmd, "CLAUDE_CODE_SHELL", &host)
        .or_else(|| inherited(cmd, "SHELL", &host))
        .filter(|s| {
            matches!(
                Path::new(s).file_name().and_then(|v| v.to_str()),
                Some("bash" | "zsh")
            )
        })
        .unwrap_or_else(|| "/bin/bash".into());
    let parent = std::path::absolute(tcr::paths::cache_dir().root().join("tool-env"))?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&parent)?;
    let meta = std::fs::symlink_metadata(&parent)?;
    let uid = tcr_fdpass::effective_uid();
    anyhow::ensure!(
        meta.is_dir() && meta.uid() == uid && meta.mode() & 0o777 == 0o700,
        "tool helper directory must be owned by this user with mode 0700 and must not be a symlink"
    );
    let directory = tempfile::Builder::new()
        .prefix("launch-")
        .tempdir_in(&parent)?;
    let root = directory.path();
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))?;
    let caller = caller_env(model_port(cmd, &host), &host);
    let mut cleanup = String::new();
    for key in PROXIES.iter().chain(CAS.iter()) {
        let saved = format!("TCR_TOOL_CALLER_{key}");
        if let Some(value) = caller.get(*key) {
            cmd.env(&saved, value);
            cleanup.push_str(&format!("export {key}=\"${{{saved}}}\"\n"));
        } else {
            cmd.env_remove(&saved);
            cleanup.push_str(&format!("unset {key}\n"));
        }
    }
    // Runtime environment only: no request key or proxy credential is written
    // into a helper file or used to choose its directory.
    if let Some(value) = inherited(cmd, "HTTPS_PROXY", &host) {
        let mut proxy = reqwest::Url::parse(&value)?;
        proxy
            .set_username(if proxy.password().is_some() {
                "tcr"
            } else {
                ""
            })
            .map_err(|()| anyhow::anyhow!("invalid nested Claude proxy URL"))?;
        cmd.env("TCR_TOOL_CLAUDE_PROXY", proxy.as_str());
    }
    cmd.env("TCR_TOOL_CLAUDE_CA", &host.own_ca);
    let prepend = format!("export PATH={}:\"$PATH\"\n", quote(path_text(root)?));
    let mut env_script = String::from("#!/bin/sh\n# tcr tool launcher\n");
    env_script.push_str(&cleanup);
    if let Some(file) = &env_file {
        env_script.push_str(&format!(". {}\n", quote(file)));
    }
    env_script.push_str(&prepend);
    publish(&root.join("shell-env"), &env_script)?;
    let exec = match prefix {
        Some(prefix) => format!("exec {} \"$@\"\n", quote(&prefix)),
        None => format!("exec {} -c \"$1\"\n", quote(&shell)),
    };
    publish(
        &root.join("shell-prefix"),
        &format!("#!/bin/sh\n# tcr tool launcher\n{cleanup}{prepend}{exec}"),
    )?;
    let restore = format!(
        "export CLAUDE_CODE_SHELL_PREFIX={}\nexport CLAUDE_ENV_FILE={}\n",
        quote(path_text(&root.join("shell-prefix"))?),
        quote(path_text(&root.join("shell-env"))?),
    );
    let mut nested_cleanup = String::new();
    for key in ["HTTPS_PROXY", "https_proxy", "NODE_EXTRA_CA_CERTS"] {
        nested_cleanup.push_str(&format!(
            "if [ \"${{{key}:-}}\" = \"${{TCR_TOOL_CALLER_{key}:-}}\" ]; then unset {key}; fi\n"
        ));
    }
    if let Some(bin) = claude {
        publish(
            &root.join("claude"),
            &format!(
                "#!/bin/sh\n# tcr tool launcher\n{restore}{nested_cleanup}: \"${{HTTPS_PROXY:=${{TCR_TOOL_CLAUDE_PROXY}}}}\"\n: \"${{https_proxy:=$HTTPS_PROXY}}\"\n: \"${{NODE_EXTRA_CA_CERTS:=${{TCR_TOOL_CLAUDE_CA}}}}\"\n: \"${{ANTHROPIC_BASE_URL:=https://api.anthropic.com}}\"\nexport HTTPS_PROXY https_proxy NODE_EXTRA_CA_CERTS ANTHROPIC_BASE_URL\nexec {} \"$@\"\n",
                quote(path_text(&bin)?),
            ),
        )?;
    }
    if let Some(bin) = codex_bin {
        let pinned = quote(path_text(&bin)?);
        publish(
            &root.join("codex"),
            &format!(
                "#!/bin/sh\n# tcr tool launcher\nif [ -n \"${{CODEX_BIN:-}}\" ] && [ \"${{TCR_CODEX_BIN:-}}\" = {pinned} ]; then unset TCR_CODEX_BIN; fi\nif [ -z \"${{TCR_CODEX_BIN:-}}${{CODEX_BIN:-}}\" ]; then export TCR_CODEX_BIN={pinned}; fi\nexec {} run --codex --config {} -- \"$@\"\n",
                quote(path_text(&tcr)?),
                quote(path_text(&config)?),
            ),
        )?;
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    cmd.env(
        "PATH",
        std::env::join_paths(
            std::iter::once(root.to_path_buf()).chain(std::env::split_paths(&path)),
        )?,
    );
    cmd.env("CLAUDE_CODE_SHELL_PREFIX", root.join("shell-prefix"));
    cmd.env("CLAUDE_ENV_FILE", root.join("shell-env"));
    Ok((
        directory,
        if claude_shell {
            vec!["[tcr] shell tools use the caller's proxy and CA settings; nested claude/codex on PATH use the pool unless explicitly overridden".into()]
        } else {
            Vec::new()
        },
    ))
}
