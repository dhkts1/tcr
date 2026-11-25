//! What the server and a local client that dials it read off a loaded config:
//! the port after a `--port` override, the account renames `load` made, and
//! the key a loopback caller presents.

use super::Config;

impl Config {
    /// Serve on `port_override` when one is given (the `--port` flag), else on
    /// the file's `proxy.port`, and answer the port this config now names.
    pub const fn apply_port_override(&mut self, port_override: Option<u16>) -> u16 {
        if let Some(port) = port_override {
            self.proxy.port = port;
        }
        self.proxy.port
    }

    /// Each rename [`super::load`] made to restore unique account names, as
    /// `(from, to)`, in the order it made them.
    pub fn rename_pairs(&self) -> Vec<(String, String)> {
        self.renamed_accounts
            .iter()
            .map(|rename| (rename.from.clone(), rename.to.clone()))
            .collect()
    }

    /// The `proxy.apiKey` a caller on loopback presents to this config's server
    /// (`x-api-key`), `None` when the server is unkeyed.
    pub fn loopback_api_key(&self) -> Option<&str> {
        self.proxy.api_key.as_deref()
    }
}
