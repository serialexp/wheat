//! TLS for connections the server accepts itself, so `wheat+tls://` reaches it
//! without a proxy in front.

use std::fmt;
use std::path::Path;
use std::sync::Arc;

use rustls_pki_types::pem::PemObject as _;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::TlsAcceptor;

/// A certificate chain or key that could not be loaded.
#[derive(Debug)]
pub struct TlsConfigError(pub String);

impl fmt::Display for TlsConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TlsConfigError {}

/// Builds what [`crate::server::ServerConfig::tls`] takes from PEM files: `cert`
/// holds the server's certificate followed by any intermediates, `key` its
/// private key (PKCS#8, PKCS#1 or SEC1).
pub fn load_pem(cert: &Path, key: &Path) -> Result<TlsAcceptor, TlsConfigError> {
    let chain = CertificateDer::pem_file_iter(cert)
        .and_then(|certs| certs.collect::<Result<Vec<_>, _>>())
        .map_err(|error| {
            TlsConfigError(format!(
                "cannot read the certificates in {}: {error}",
                cert.display()
            ))
        })?;
    if chain.is_empty() {
        return Err(TlsConfigError(format!(
            "{} holds no PEM certificate",
            cert.display()
        )));
    }
    let private_key = PrivateKeyDer::from_pem_file(key).map_err(|error| {
        TlsConfigError(format!(
            "cannot read a private key from {}: {error}",
            key.display()
        ))
    })?;
    acceptor(chain, private_key)
}

/// Builds an acceptor that presents `chain` and proves it holds `key`.
pub fn acceptor(
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Result<TlsAcceptor, TlsConfigError> {
    // Named, rather than the process default, so an embedder that enables a second
    // provider cannot make the default ambiguous.
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| TlsConfigError(format!("cannot set up TLS: {error}")))?
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .map_err(|error| {
            TlsConfigError(format!(
                "the certificate and key do not make a usable pair: {error}"
            ))
        })?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}
