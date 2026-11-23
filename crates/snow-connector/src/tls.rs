//! TLS trust for every connection `snow` makes: the operating system's
//! certificate store, so corporate root CAs and TLS-inspecting proxies work.
//! `SSL_CERT_FILE` and `SSL_CERT_DIR` replace the system store when set.

use std::sync::{Arc, LazyLock};

/// Loaded on the first TLS connection, not at startup: reading the system
/// store can take tens of milliseconds.
static ROOTS: LazyLock<Arc<rustls::RootCertStore>> = LazyLock::new(|| {
    let loaded = rustls_native_certs::load_native_certs();
    let mut roots = rustls::RootCertStore::empty();
    roots.add_parsable_certificates(loaded.certs);
    if roots.is_empty() {
        let cause = loaded
            .errors
            .first()
            .map(|e| format!(" ({e})"))
            .unwrap_or_default();
        eprintln!(
            "Warning: no trusted CA certificates were found in the operating system's certificate store{cause}, so every TLS connection will fail.\nInstall the system CA bundle (for example the ca-certificates package), or set SSL_CERT_FILE to a PEM file or SSL_CERT_DIR to a directory of PEM certificates."
        );
    }
    Arc::new(roots)
});

static DEFAULT: LazyLock<Arc<rustls::ClientConfig>> = LazyLock::new(|| {
    Arc::new(config(rustls::DEFAULT_VERSIONS).expect("ring supports the default TLS versions"))
});

/// A client config with the `ring` provider, limited to `versions`.
pub(crate) fn config(
    versions: &[&'static rustls::SupportedProtocolVersion],
) -> Result<rustls::ClientConfig, rustls::Error> {
    Ok(rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(versions)?
    .with_root_certificates(ROOTS.clone())
    .with_no_client_auth())
}

/// The shared client config for TLS 1.2 and 1.3.
pub(crate) fn default_config() -> Arc<rustls::ClientConfig> {
    DEFAULT.clone()
}

/// Every HTTP client starts here, so all of them trust the same roots. ureq's
/// own constructors would trust its bundled Mozilla roots instead, so
/// `clippy.toml` disallows them everywhere else.
#[allow(clippy::disallowed_methods)]
pub fn agent_builder() -> ureq::AgentBuilder {
    ureq::AgentBuilder::new().tls_config(default_config())
}
