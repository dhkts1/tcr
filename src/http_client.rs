//! Where every `reqwest` client in tcr starts.
//!
//! reqwest 0.13 picks its rustls crypto provider at `build()` time: the
//! process-wide default if one is installed, otherwise its own, and its own is
//! aws-lc-rs, a C library this repository does not build. tcr compiles reqwest
//! with `rustls-no-provider` and uses ring, the provider `rustls` and
//! `tokio-rustls` are already built with here. With no default installed,
//! `ClientBuilder::build` panics ("No rustls crypto provider is configured"), so
//! [`builder`] installs ring first and every call site goes through it.
//! `clippy.toml` disallows `reqwest::Client::builder`, `reqwest::Client::new`
//! and `reqwest::get` everywhere else, so a new site cannot skip it.
//!
//! Server certificates are checked by `rustls-platform-verifier`, reqwest
//! 0.13's only rustls verifier when no `tls_certs_only` is given: the system
//! trust store (Security.framework on macOS, the distribution's CA bundle on
//! Linux) instead of the Mozilla roots reqwest 0.12 compiled in.

use std::sync::Once;

/// Install ring as the process-wide rustls crypto provider, once.
///
/// Two places need it: a `reqwest` client (see the module doc) and the MITM
/// listener's `ServerConfig`, whose `builder()` panics when no default is set.
/// Another provider already installed is not an error: rustls then uses that
/// one, and there is nothing to choose between.
pub fn install_crypto_provider() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        if rustls::crypto::ring::default_provider()
            .install_default()
            .is_err()
        {
            tracing::debug!("a rustls crypto provider was already installed");
        }
    });
}

/// `reqwest::Client::builder()`, with the crypto provider installed first.
#[expect(
    clippy::disallowed_methods,
    reason = "the one place reqwest::Client::builder is called"
)]
pub fn builder() -> reqwest::ClientBuilder {
    install_crypto_provider();
    reqwest::Client::builder()
}

#[cfg(test)]
mod tests {
    /// A client built through [`super::builder`]. Without the install, the
    /// build panics under reqwest 0.13 with `rustls-no-provider` in any process
    /// where nothing else installed a provider first.
    #[test]
    fn a_client_builds_through_builder() {
        assert!(super::builder().no_proxy().build().is_ok());
    }
}
