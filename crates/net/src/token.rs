//! Join tokens: `FC1-<secret>-<hub fingerprint prefix>` (base32).
//!
//! The token carries the first 16 bytes of the hub's certificate
//! fingerprint, so an agent verifies the hub *before* revealing the secret
//! on its first connection (no trust-on-first-use window). After joining the
//! agent stores the full fingerprint.

use data_encoding::BASE32_NOPAD;
use flow_proto::Fingerprint;
use subtle::ConstantTimeEq;

pub const SECRET_LEN: usize = 16;
pub const FP_PREFIX_LEN: usize = 16;

#[derive(Clone, PartialEq, Eq)]
pub struct JoinToken {
    pub secret: [u8; SECRET_LEN],
    pub hub_fp_prefix: [u8; FP_PREFIX_LEN],
}

impl std::fmt::Debug for JoinToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("JoinToken(<redacted>)")
    }
}

impl JoinToken {
    pub fn new(secret: [u8; SECRET_LEN], hub: &Fingerprint) -> Self {
        let mut p = [0u8; FP_PREFIX_LEN];
        p.copy_from_slice(&hub.0[..FP_PREFIX_LEN]);
        JoinToken { secret, hub_fp_prefix: p }
    }

    pub fn random_secret() -> [u8; SECRET_LEN] {
        rand::random()
    }

    pub fn encode(&self) -> String {
        format!("FC1-{}-{}", BASE32_NOPAD.encode(&self.secret), BASE32_NOPAD.encode(&self.hub_fp_prefix))
    }

    pub fn parse(s: &str) -> Option<Self> {
        let clean: String = s.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_ascii_uppercase();
        let mut it = clean.split('-');
        if it.next()? != "FC1" {
            return None;
        }
        let secret = BASE32_NOPAD.decode(it.next()?.as_bytes()).ok()?;
        let fp = BASE32_NOPAD.decode(it.next()?.as_bytes()).ok()?;
        if it.next().is_some() || secret.len() != SECRET_LEN || fp.len() != FP_PREFIX_LEN {
            return None;
        }
        Some(JoinToken { secret: secret.try_into().ok()?, hub_fp_prefix: fp.try_into().ok()? })
    }

    /// Constant-time check of the secret part.
    pub fn secret_matches(&self, secret: &[u8; SECRET_LEN]) -> bool {
        bool::from(self.secret.ct_eq(secret))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_tolerant_parsing() {
        let fp = Fingerprint([0x5a; 32]);
        let t = JoinToken::new([1; 16], &fp);
        let s = t.encode();
        assert!(s.starts_with("FC1-"));
        assert_eq!(JoinToken::parse(&s), Some(t.clone()));
        assert_eq!(JoinToken::parse(&format!("  {}\n", s.to_lowercase())), Some(t.clone()));
        assert!(t.secret_matches(&[1; 16]));
        assert!(!t.secret_matches(&[2; 16]));
    }

    #[test]
    fn rejects_garbage() {
        for bad in ["", "FC1", "FC2-AAAA-BBBB", "FC1-AAAA-BBBB", "FC1-!!-??", "FC1-AE-AE-AE"] {
            assert_eq!(JoinToken::parse(bad), None, "{bad}");
        }
        let fp = Fingerprint([0; 32]);
        let s = JoinToken::new([1; 16], &fp).encode();
        assert_eq!(JoinToken::parse(&format!("{s}-X")), None);
    }
}
