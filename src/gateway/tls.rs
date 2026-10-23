//! The gateway's certificate: PEM files that it reads again when they change, so a renewed
//! certificate needs no restart. They come from systemd credentials (certbot's files), from
//! paths given as flags, or from `tailscale cert` for a host without a domain of its own.

use anyhow::{anyhow, bail, Context, Result};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

/// The modification times of the certificate and the key.
type Stamp = (Option<SystemTime>, Option<SystemTime>);

pub struct Reloading {
    cert: PathBuf,
    key: PathBuf,
    provider: Arc<CryptoProvider>,
    /// The loaded key, and the stamp that was last tried.
    state: Mutex<(Arc<CertifiedKey>, Stamp)>,
}

impl std::fmt::Debug for Reloading {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reloading").field("cert", &self.cert).field("key", &self.key).finish()
    }
}

fn stamp(cert: &Path, key: &Path) -> Stamp {
    let time = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    (time(cert), time(key))
}

pub fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn load(cert: &Path, key: &Path, provider: &CryptoProvider) -> Result<CertifiedKey> {
    let chain = CertificateDer::pem_file_iter(cert)
        .with_context(|| format!("reading {}", cert.display()))?
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("reading {}", cert.display()))?;
    if chain.is_empty() {
        bail!("{} has no certificate", cert.display());
    }
    let key_der = PrivateKeyDer::from_pem_file(key).with_context(|| format!("reading the key {}", key.display()))?;
    CertifiedKey::from_der(chain, key_der, provider).map_err(|e| anyhow!("{} and {}: {e}", cert.display(), key.display()))
}

impl Reloading {
    pub fn new(cert: PathBuf, key: PathBuf, provider: Arc<CryptoProvider>) -> Result<Self> {
        let at = stamp(&cert, &key);
        let loaded = load(&cert, &key, &provider)?;
        Ok(Reloading { cert, key, provider, state: Mutex::new((Arc::new(loaded), at)) })
    }

    /// The current key, read again when a file changed. A pair that does not load (one file
    /// written, the other not yet) keeps the old key until the files change again.
    pub fn current(&self) -> Arc<CertifiedKey> {
        let at = stamp(&self.cert, &self.key);
        let mut state = self.state.lock().unwrap();
        if at != state.1 {
            state.1 = at;
            match load(&self.cert, &self.key, &self.provider) {
                Ok(loaded) => {
                    state.0 = Arc::new(loaded);
                    eprintln!("layr gateway: loaded the certificate {}", self.cert.display());
                }
                Err(e) => eprintln!("layr gateway: kept the old certificate: {e:#}"),
            }
        }
        state.0.clone()
    }
}

impl ResolvesServerCert for Reloading {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.current())
    }
}

/// A blocking HTTP/1.1 server config with the reloadable certificate.
pub fn server(resolver: Arc<Reloading>) -> Result<Arc<rustls::ServerConfig>> {
    let mut config = rustls::ServerConfig::builder_with_provider(resolver.provider.clone())
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_cert_resolver(resolver);
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    fn pair(home: &Path, name: &str) -> (PathBuf, PathBuf) {
        let cert = home.join(format!("{name}.crt"));
        let key = home.join(format!("{name}.key"));
        let status = Command::new("openssl")
            .args([
                "req",
                "-x509",
                "-newkey",
                "ec",
                "-pkeyopt",
                "ec_paramgen_curve:P-256",
                "-nodes",
                "-days",
                "1",
                "-subj",
                "/CN=localhost",
                "-keyout",
            ])
            .arg(&key)
            .arg("-out")
            .arg(&cert)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        (cert, key)
    }
    #[test]
    fn certificate_reload_keeps_valid_pair_until_replacement_is_complete() {
        let home = std::env::temp_dir().join(format!("layr-gateway-tls-{:016x}", rand::random::<u64>()));
        fs::create_dir(&home).unwrap();
        let (cert, key) = pair(&home, "current");
        let (next_cert, next_key) = pair(&home, "next");
        let resolver = Reloading::new(cert.clone(), key.clone(), provider()).unwrap();
        let first = resolver.current().cert[0].clone();
        fs::rename(next_cert, &cert).unwrap();
        assert_eq!(resolver.current().cert[0], first);
        fs::rename(next_key, &key).unwrap();
        assert_ne!(resolver.current().cert[0], first);
        fs::remove_dir_all(home).unwrap();
    }
}
