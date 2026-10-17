//! The old names for the library's own fixtures, for the files that still call them.
//!
//! This was a hand copy of `src/test_support.rs`, which `#[cfg(test)]` used to hide from the
//! integration tests. The `test-support` feature now exposes that module to `tests/` (see the self
//! dev-dependency in `Cargo.toml`), so these two names only point at the one copy:
//! `Account::fixture` and `Config::fixture`. New code calls those directly.
use tcr::config::{Account, Config};

/// [`Account::fixture`].
pub fn fixture_account(name: &str) -> Account {
    Account::fixture(name)
}

/// [`Config::fixture`].
pub fn fixture_config(accounts: Vec<Account>) -> Config {
    Config::fixture(accounts)
}
