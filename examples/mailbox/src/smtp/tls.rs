//! The certificate a listener offers `STARTTLS` with: a PEM certificate chain and its key, as a
//! TLS server over the `ring` provider.

use std::path::Path;
use std::sync::Arc;

use thiserror::Error;
use tokio_rustls::rustls;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::CertificateDer;
use tokio_rustls::rustls::pki_types::PrivateKeyDer;
use tokio_rustls::TlsAcceptor;

/// Why the certificate could not be loaded.
#[derive(Debug, Error)]
pub enum TlsError {
    /// A PEM file could not be read or parsed.
    #[error("{0}")]
    Pem(#[from] rustls::pki_types::pem::Error),
    /// The certificate and key do not make a TLS server.
    #[error("{0}")]
    Rustls(#[from] rustls::Error),
}

/// The TLS server presenting the chain in `certificates` with the private key in `key`, both
/// PEM files.
pub fn acceptor(certificates: &Path, key: &Path) -> Result<TlsAcceptor, TlsError> {
    let chain = CertificateDer::pem_file_iter(certificates)?.collect::<Result<Vec<_>, _>>()?;
    let key = PrivateKeyDer::from_pem_file(key)?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(chain, key)?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}
