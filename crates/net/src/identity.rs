//! Persistent node identity: an ECDSA P-256 key pair with a self-signed
//! certificate. The private key never leaves the machine; peers only learn
//! the certificate fingerprint (SHA-256 of its DER encoding).

use anyhow::{Context, Result};
use flow_proto::Fingerprint;
use quinn::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};
use std::path::Path;

#[derive(Clone)]
pub struct Identity {
    pub cert: CertificateDer<'static>,
    key_pkcs8: Vec<u8>,
    pub fingerprint: Fingerprint,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity").field("fingerprint", &self.fingerprint).finish_non_exhaustive()
    }
}

pub fn fingerprint_of(cert: &[u8]) -> Fingerprint {
    Fingerprint(Sha256::digest(cert).into())
}

impl Identity {
    /// Generates a fresh identity. `name` ends up in the certificate's SAN
    /// (informational only; peers are verified by fingerprint).
    pub fn generate(name: &str) -> Result<Self> {
        let san = sanitize_dns(name);
        let ck = rcgen::generate_simple_self_signed(vec![san]).context("generating certificate")?;
        let cert = ck.cert.der().clone();
        let key_pkcs8 = ck.signing_key.serialize_der();
        let fingerprint = fingerprint_of(&cert);
        Ok(Identity { cert, key_pkcs8, fingerprint })
    }

    pub fn from_der(cert: Vec<u8>, key_pkcs8: Vec<u8>) -> Self {
        let fingerprint = fingerprint_of(&cert);
        Identity { cert: CertificateDer::from(cert), key_pkcs8, fingerprint }
    }

    pub fn key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key_pkcs8.clone()))
    }

    /// Loads `<dir>/<stem>.crt` + `<dir>/<stem>.key` (DER), creating them if
    /// missing. The key file is written with mode 0600 on Unix.
    pub fn load_or_create(dir: &Path, stem: &str, name: &str) -> Result<Self> {
        let crt = dir.join(format!("{stem}.crt"));
        let key = dir.join(format!("{stem}.key"));
        if crt.exists() && key.exists() {
            let c = std::fs::read(&crt).with_context(|| format!("reading {}", crt.display()))?;
            let k = std::fs::read(&key).with_context(|| format!("reading {}", key.display()))?;
            return Ok(Identity::from_der(c, k));
        }
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let id = Identity::generate(name)?;
        write_private(&key, &id.key_pkcs8)?;
        std::fs::write(&crt, id.cert.as_ref()).with_context(|| format!("writing {}", crt.display()))?;
        Ok(id)
    }
}

/// Writes a file readable only by the current user (on Unix).
pub fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("renaming to {}", path.display()))?;
    Ok(())
}

fn sanitize_dns(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c.to_ascii_lowercase() } else { '-' })
        .collect();
    let s = s.trim_matches('-');
    if s.is_empty() {
        "flowcursor".into()
    } else {
        s.chars().take(63).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_and_persist() {
        let dir = std::env::temp_dir().join(format!("flow-id-test-{}", rand::random::<u64>()));
        let a = Identity::load_or_create(&dir, "agent", "My PC!").unwrap();
        let b = Identity::load_or_create(&dir, "agent", "ignored").unwrap();
        assert_eq!(a.fingerprint, b.fingerprint);
        let c = Identity::generate("other").unwrap();
        assert_ne!(a.fingerprint, c.fingerprint);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("agent.key")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn dns_names() {
        assert_eq!(sanitize_dns("My PC!"), "my-pc");
        assert_eq!(sanitize_dns("!!!"), "flowcursor");
    }
}
