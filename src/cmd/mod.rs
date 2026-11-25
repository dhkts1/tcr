//! The binary's verb modules: each runs one `tcr` verb and owns that verb's clap `Args`.
//! [`ConfigFile`], the `--config` flag nearly every verb flattens in, lives here because they all
//! share it.

use std::path::PathBuf;

use tcr::base::config;

pub(crate) mod peer;
pub(crate) mod run;
pub(crate) mod server;
#[cfg(test)]
mod test_support;
pub(crate) mod tool_env;

/// `--config` on every verb that takes it with the stock help line, resolved
/// in one place.
#[derive(clap::Args, Clone, Debug)]
pub(crate) struct ConfigFile {
    /// Path to the config file.
    #[arg(long)]
    config: Option<PathBuf>,
}

impl ConfigFile {
    /// The path given, or the default config path.
    pub(crate) fn path(&self) -> PathBuf {
        self.config.clone().unwrap_or_else(config::default_path)
    }
}
