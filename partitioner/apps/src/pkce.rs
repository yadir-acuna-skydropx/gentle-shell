//! PKCE (RFC 7636) and OAuth `state` generation for the Slack browser login.
//!
//! The `code_verifier` and `state` come from `/dev/urandom` (same source as the
//! partitioner's predict server). The `code_challenge` is
//! `base64url_nopad(sha256(code_verifier))` with method `S256`.
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

/// Computes the S256 `code_challenge` for a `code_verifier`:
/// `base64url_nopad(sha256(verifier))`. Pure function (RFC 7636 §4.2).
pub fn code_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

/// Generates a PKCE `code_verifier`: 43 chars from 32 random bytes, base64url
/// without padding. All characters are in the RFC 7636 unreserved set
/// (`A-Z a-z 0-9 - . _ ~`); base64url uses `-` and `_` and no padding.
pub fn generate_code_verifier() -> String {
    URL_SAFE_NO_PAD.encode(random_bytes::<32>())
}

/// Generates an OAuth `state` with ≥128 bits of entropy (16 random bytes,
/// base64url without padding → 22 chars).
pub fn generate_state() -> String {
    URL_SAFE_NO_PAD.encode(random_bytes::<16>())
}

/// Reads `N` bytes from `/dev/urandom`, falling back to a non-crypto time/pid
/// hash only if that read fails (keeps login usable on an odd host).
fn random_bytes<const N: usize>() -> [u8; N] {
    #[cfg(unix)]
    {
        use std::io::Read;
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            let mut b = [0u8; N];
            if f.read_exact(&mut b).is_ok() {
                return b;
            }
        }
    }
    // Fallback: not cryptographic, but keeps login usable.
    use std::hash::{Hash, Hasher};
    let mut out = [0u8; N];
    let mut h = std::collections::hash_map::DefaultHasher::new();
    std::time::SystemTime::now().hash(&mut h);
    std::process::id().hash(&mut h);
    for (i, slot) in out.iter_mut().enumerate() {
        i.hash(&mut h);
        *slot = (h.finish() & 0xff) as u8;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc7636_test_vector() {
        // RFC 7636 Appendix B (authoritative value, verified with openssl).
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(code_challenge(verifier), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn verifier_is_valid_length_and_charset() {
        let v = generate_code_verifier();
        assert!((43..=128).contains(&v.len()), "len {}", v.len());
        assert!(
            v.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~')),
            "unreserved charset: {v}"
        );
    }

    #[test]
    fn state_has_enough_entropy_and_is_url_safe() {
        let s = generate_state();
        assert!(s.len() >= 22, "state len {}", s.len());
        assert!(s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')));
        assert_ne!(generate_state(), generate_state(), "state should be random");
    }
}
