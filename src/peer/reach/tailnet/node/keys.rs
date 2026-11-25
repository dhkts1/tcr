//! The node's auth key and its key file, `tcr-tailnet.key`.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};

/// The key file's name, in the config directory beside `tcr-node.key`.
pub const KEY_FILE_NAME: &str = "tcr-tailnet.key";

/// `<config_dir>/tcr-tailnet.key`.
pub fn key_path(config_dir: &Path) -> PathBuf {
    config_dir.join(KEY_FILE_NAME)
}

/// The key file that goes with a peers file: in the same directory, the way
/// the server finds `tcr-node.key`.
pub fn key_path_for_peers(peers_path: &Path) -> PathBuf {
    key_path(
        &peers_path
            .parent()
            .map_or_else(crate::peer::id::default_config_dir, Path::to_path_buf),
    )
}

// ---------------------------------------------------------------------------
// The auth key
// ---------------------------------------------------------------------------

/// A Tailscale auth key, read once from stdin.
///
/// No `Display`, a `Debug` that prints nothing of it, no `Serialize`: the only
/// way out is [`Self::into_secret`], called where the library takes it.
pub struct AuthKey(String);

impl AuthKey {
    /// Wrap a key read from stdin. Refuses an empty one.
    pub fn new(raw: &str) -> Result<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            bail!("peer tailnet: no auth key on standard input");
        }
        Ok(Self(trimmed.to_string()))
    }

    /// The key itself, for the library and nothing else.
    pub fn into_secret(self) -> String {
        self.0
    }
}

impl std::fmt::Debug for AuthKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AuthKey(..)")
    }
}

// ---------------------------------------------------------------------------
// The key file
// ---------------------------------------------------------------------------

/// What `tcr-tailnet.key` holds: the node's keys and the name it asked for.
///
/// `key_state` is the library's `PersistState` exactly as the library
/// serializes it, kept opaque here so a build without the library can still
/// check, move and delete the file. The same shape as the library's own key
/// file (`{"key_state": ...}`), so either can read the other's.
#[derive(Clone, Serialize, Deserialize)]
pub struct NodeKeyFile {
    /// The node, machine and network-lock keys. Secret.
    pub key_state: serde_json::Value,
    /// The name this node asks control for, `--name` at join.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
}

impl std::fmt::Debug for NodeKeyFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeKeyFile")
            .field("key_state", &"..")
            .field("hostname", &self.hostname)
            .finish()
    }
}

/// Read the key file at `path`, or `None` when there is none.
///
/// Refuses a file with a mode other than 0600, the rule `tcr-node.key` follows
/// (`crate::peer::id::PRIVATE_KEY_MODE`): a wider mode means something other
/// than tcr wrote it, and the file IS the node's identity on the tailnet.
pub fn read_key_file(path: &Path) -> Result<Option<NodeKeyFile>> {
    use std::os::unix::fs::PermissionsExt as _;

    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };
    if !meta.file_type().is_file() {
        bail!(
            "{} is not a regular file: refusing to trust a tailnet key file this program did \
             not write",
            path.display()
        );
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode != crate::peer::id::PRIVATE_KEY_MODE {
        bail!(
            "{} has mode {mode:o}, expected {:o}: refusing to trust a tailnet key file this \
             program did not write",
            path.display(),
            crate::peer::id::PRIVATE_KEY_MODE
        );
    }
    let raw = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let file: NodeKeyFile = serde_json::from_slice(&raw)
        .with_context(|| format!("{} is not a tailnet key file", path.display()))?;
    Ok(Some(file))
}

/// Write the key file at `path`, mode 0600, refusing one that already exists.
///
/// Through the writer `tcr-node.key` uses, never the library's (which leaves
/// the process umask, 0644: upstream #449), and never over an existing file,
/// so the keys are never re-minted behind the operator's back.
pub fn write_key_file(path: &Path, file: &NodeKeyFile) -> Result<()> {
    if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let bytes = serde_json::to_vec(file).context("serializing the tailnet key file")?;
    crate::peer::id::write_key_file(path, &bytes, crate::peer::id::PRIVATE_KEY_MODE)
}

/// Delete the key file at `path`. Reports whether there was one.
pub fn remove_key_file(path: &Path) -> Result<bool> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err).with_context(|| format!("deleting {}", path.display())),
    }
}

/// Refuse a `--name` control would not accept as a machine name: 1 to 63
/// letters, digits and hyphens, not starting or ending with a hyphen.
pub fn check_node_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 63
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-');
    if !ok {
        bail!(
            "peer tailnet: `{name}` is not a machine name; use 1 to 63 letters, digits and \
             hyphens, for example `tcr-studio`"
        );
    }
    Ok(())
}
