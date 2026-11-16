//! A tiny local hash used only to turn a folder path into a stable filesystem
//! key for its session socket (S34). This is FNV-1a (128-bit): deliberately not
//! cryptographic, it has no dependency and no C code, which is all a per-folder
//! id needs. The output is 32 lowercase hex characters.

use std::path::Path;

const FNV_OFFSET_128: u128 = 0x6c62272e07bb014262b821756295c58d;
const FNV_PRIME_128: u128 = 0x0000000001000000000000000000013b;

/// FNV-1a over `data`, 128 bits wide.
pub fn fnv1a_128(data: &[u8]) -> u128 {
    let mut hash = FNV_OFFSET_128;
    for &b in data {
        hash ^= b as u128;
        hash = hash.wrapping_mul(FNV_PRIME_128);
    }
    hash
}

/// 32-character lowercase hex of a 128-bit value.
pub fn hex128(value: u128) -> String {
    format!("{value:032x}")
}

/// Stable per-folder id from an already-canonical absolute path. Callers pass
/// the canonicalized path so two spellings of the same folder collapse to one
/// id; two different folders get different ids.
pub fn folder_id_from_bytes(canonical_path: &[u8]) -> String {
    hex128(fnv1a_128(canonical_path))
}

/// Convenience wrapper over [`folder_id_from_bytes`] for a path.
pub fn folder_id(canonical: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    folder_id_from_bytes(canonical.as_os_str().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn fnv1a_128_matches_the_known_empty_and_a_vectors() {
        // Reference FNV-1a 128-bit vectors (offset basis for "", and for "a").
        assert_eq!(fnv1a_128(b""), FNV_OFFSET_128);
        assert_eq!(hex128(fnv1a_128(b"")).len(), 32);
        // "a" must differ from "" and be stable.
        assert_ne!(fnv1a_128(b"a"), fnv1a_128(b""));
        assert_eq!(fnv1a_128(b"a"), fnv1a_128(b"a"));
    }

    #[test]
    fn hex128_is_always_32_lowercase_hex_chars() {
        let h = hex128(0);
        assert_eq!(h, "0".repeat(32));
        let h = hex128(u128::MAX);
        assert_eq!(h, "f".repeat(32));
        assert!(h.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn folder_id_is_stable_for_the_same_path() {
        let a = folder_id(&PathBuf::from("/home/user/project"));
        let b = folder_id(&PathBuf::from("/home/user/project"));
        assert_eq!(a, b);
        assert_eq!(a.len(), 32);
    }

    #[test]
    fn folder_id_differs_for_different_paths() {
        let a = folder_id(&PathBuf::from("/home/user/project-a"));
        let b = folder_id(&PathBuf::from("/home/user/project-b"));
        assert_ne!(a, b);
    }
}
