//! Where every `reqwest` client in tcr starts.
//!
//! Two things every client needs, done once here:
//!
//! - **A crypto provider.** reqwest 0.13 picks its rustls crypto provider at
//!   `build()` time: the process-wide default if one is installed, otherwise
//!   its own, and its own is aws-lc-rs, a C library this repository does not
//!   build. tcr compiles reqwest with `rustls-no-provider` and uses ring, the
//!   provider `rustls` and `tokio-rustls` are already built with here. With no
//!   default installed, `ClientBuilder::build` panics ("No rustls crypto
//!   provider is configured"), so [`builder`] installs ring first.
//! - **The bundled Mozilla roots, and only those.** reqwest 0.12 compiled the
//!   Mozilla root set in; 0.13 checks servers against the OS trust store
//!   (`rustls-platform-verifier`) unless it is given `tls_certs_only`. tcr
//!   keeps the bundled set on every OS (Gil, 2026-10-17), so a server is
//!   trusted the same way on a Mac, on a Linux host with no CA bundle, and
//!   behind whatever a machine's keychain has been taught to trust. A caller
//!   that adds its own root (`add_root_certificate`, tests only) adds it to
//!   this set.
//!
//! `clippy.toml` disallows `reqwest::Client::builder`, `reqwest::Client::new`
//! and `reqwest::get` everywhere else, so a new site cannot skip it.

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

/// `reqwest::Client::builder()`, with the crypto provider installed and the
/// bundled Mozilla roots as the only trust anchors.
#[expect(
    clippy::disallowed_methods,
    reason = "the one place reqwest::Client::builder is called"
)]
pub fn builder() -> reqwest::ClientBuilder {
    install_crypto_provider();
    reqwest::Client::builder().tls_certs_only(bundled_roots())
}

/// The Mozilla root set from `webpki-root-certs`, as reqwest certificates.
///
/// With only the rustls backend compiled, `Certificate::from_der` copies the
/// bytes and cannot fail; a failure is still logged rather than dropped
/// silently, because a root missing from this set is a server tcr will refuse.
fn bundled_roots() -> impl Iterator<Item = reqwest::Certificate> {
    webpki_root_certs::TLS_SERVER_ROOT_CERTS
        .iter()
        .filter_map(|der| match reqwest::Certificate::from_der(der) {
            Ok(cert) => Some(cert),
            Err(err) => {
                tracing::error!(error = %err, "a bundled root certificate did not load");
                None
            }
        })
}

#[cfg(test)]
mod tests {
    /// Every bundled root loads, so `builder` trusts the whole Mozilla set.
    #[test]
    fn every_bundled_root_loads() {
        assert_eq!(
            super::bundled_roots().count(),
            webpki_root_certs::TLS_SERVER_ROOT_CERTS.len()
        );
    }

    /// A server whose certificate no bundled root issued is refused by
    /// webpki's own verifier, not by the OS trust store.
    ///
    /// The two report the refusal differently, which is what this reads:
    /// `WebPkiServerVerifier` (what `tls_certs_only` selects) says
    /// `CertificateError::UnknownIssuer`, while `rustls-platform-verifier` on
    /// macOS hands back Security.framework's verdict as `CertificateError::Other`
    /// ("certificate is not trusted: -67843"). Measured both ways on macOS:
    /// with `tls_certs_only` removed from [`super::builder`] this test fails on
    /// that `Other`. On Linux the platform verifier is also webpki underneath,
    /// so there this proves only the refusal, not which store refused.
    #[tokio::test]
    async fn an_unknown_issuer_is_refused_by_webpki_not_the_os_store() {
        use std::sync::Arc;

        let generated = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let cert = rustls::pki_types::CertificateDer::from(generated.cert.der().to_vec());
        let key = rustls::pki_types::PrivateKeyDer::try_from(generated.signing_key.serialize_der())
            .unwrap();
        super::install_crypto_provider();
        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert], key)
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            // The client refuses the handshake; the server's half of it fails.
            assert!(acceptor.accept(stream).await.is_err());
        });

        let client = super::builder()
            .no_proxy()
            .resolve("localhost", addr)
            .build()
            .unwrap();
        let err = client
            .get(format!("https://localhost:{}/", addr.port()))
            .send()
            .await
            .expect_err("a self-signed server must be refused");

        let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&err);
        let mut found = None;
        while let Some(current) = source {
            if let Some(tls) = current.downcast_ref::<rustls::Error>() {
                found = Some(tls.clone());
                break;
            }
            // `io::Error::source` skips the error it wraps and returns that
            // error's own source, so step into the wrapped one by hand.
            source = match current.downcast_ref::<std::io::Error>() {
                Some(io) => io
                    .get_ref()
                    .map(|inner| inner as &(dyn std::error::Error + 'static)),
                None => current.source(),
            };
        }
        assert_eq!(
            found,
            Some(rustls::Error::InvalidCertificate(
                rustls::CertificateError::UnknownIssuer
            )),
            "the refusal must come from webpki over the bundled roots: {err:?}"
        );
    }

    /// A client built through [`super::builder`]. Without the provider install,
    /// the build panics under reqwest 0.13 with `rustls-no-provider` in any
    /// process where nothing else installed a provider first.
    #[test]
    fn a_client_builds_through_builder() {
        assert!(super::builder().no_proxy().build().is_ok());
    }
}
