//! Self-signed server identity, created on first run and pinned by clients.

use std::fs;
use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};

#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("certificate file {path}: {source}")]
    Io { path: String, source: io::Error },
    #[error("generate certificate: {0}")]
    Generate(#[from] rcgen::Error),
}

pub struct Identity {
    pub cert: CertificateDer<'static>,
    pub key: PrivateKeyDer<'static>,
    pub fingerprint: String,
}

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> TlsError + '_ {
    move |source| TlsError::Io { path: path.display().to_string(), source }
}

pub fn fingerprint(der: &[u8]) -> String {
    Sha256::digest(der).iter().map(|b| format!("{b:02x}")).collect()
}

/// Loads `cert.der`/`key.der` from `dir`, generating them on first run.
pub fn load_or_create(dir: &Path) -> Result<Identity, TlsError> {
    let cert_path = dir.join("cert.der");
    let key_path = dir.join("key.der");
    let fp_path = dir.join("cert.sha256");
    if !(cert_path.exists() && key_path.exists()) {
        fs::create_dir_all(dir).map_err(io_err(dir))?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(io_err(dir))?;
        let ck = rcgen::generate_simple_self_signed(vec![homeport_proto::SERVER_NAME.to_owned()])?;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&key_path)
            .map_err(io_err(&key_path))?;
        io::Write::write_all(&mut f, &ck.signing_key.serialize_der()).map_err(io_err(&key_path))?;
        fs::write(&cert_path, ck.cert.der()).map_err(io_err(&cert_path))?;
        tracing::info!(path = %cert_path.display(), "generated server certificate");
    }
    let cert = fs::read(&cert_path).map_err(io_err(&cert_path))?;
    let key = fs::read(&key_path).map_err(io_err(&key_path))?;
    let fingerprint = fingerprint(&cert);
    fs::write(&fp_path, format!("{fingerprint}\n")).map_err(io_err(&fp_path))?;
    Ok(Identity {
        cert: CertificateDer::from(cert),
        key: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)),
        fingerprint,
    })
}
