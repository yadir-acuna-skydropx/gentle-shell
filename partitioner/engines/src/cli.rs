//! Key-management logic for `gentle-explain key …`, split from the binary so it
//! is testable with an in-memory store and a fake API. No function here prints
//! or returns the key.
use crate::anthropic::{AnthropicError, Client};
use crate::keystore::{self, KeyStore};

/// Result of `key set`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetOutcome {
    /// Stored successfully; the message is user-facing.
    Saved,
    /// The key failed the surface-format check; carries a key-free reason.
    InvalidFormat(String),
    /// The key was syntactically fine but Anthropic rejected it (401).
    Rejected,
    /// A storage or network failure; carries a key-free message.
    Error(String),
}

/// Validates `key` (format, then a cheap `GET /v1/models` against `base`) and
/// stores it on success. `key` is consumed only through the store; it is never
/// echoed in any returned value.
pub fn key_set(store: &dyn KeyStore, key: &str, base: &str) -> SetOutcome {
    let key = key.trim();
    if let Err(e) = keystore::validate_format(key) {
        return SetOutcome::InvalidFormat(e);
    }
    let client = Client::new(key, base);
    match client.validate_key() {
        Ok(()) => {}
        Err(AnthropicError::Auth) => return SetOutcome::Rejected,
        Err(e) => return SetOutcome::Error(e.to_string()),
    }
    match store.save(key) {
        Ok(()) => SetOutcome::Saved,
        Err(e) => SetOutcome::Error(e),
    }
}

/// The exact `key status` line ("API key: set" / "API key: not set").
pub fn key_status_line(store: &dyn KeyStore) -> Result<&'static str, String> {
    match store.load()? {
        Some(_) => Ok("API key: set"),
        None => Ok("API key: not set"),
    }
}

/// Removes the stored key. A missing key is not an error.
pub fn key_clear(store: &dyn KeyStore) -> Result<(), String> {
    store.delete()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keystore::MemoryStore;

    #[test]
    fn invalid_prefix_is_rejected_without_network_or_storage() {
        let store = MemoryStore::new();
        // base points nowhere; format check must fail first (no request made).
        let out = key_set(&store, "nope-123456789", "http://127.0.0.1:1");
        assert!(matches!(out, SetOutcome::InvalidFormat(ref m) if m.contains("sk-ant-")));
        assert_eq!(store.load().unwrap(), None);
    }

    #[test]
    fn status_and_clear_reflect_the_store() {
        let store = MemoryStore::new();
        assert_eq!(key_status_line(&store).unwrap(), "API key: not set");
        store.save("sk-ant-abcdefgh").unwrap();
        assert_eq!(key_status_line(&store).unwrap(), "API key: set");
        key_clear(&store).unwrap();
        assert_eq!(key_status_line(&store).unwrap(), "API key: not set");
    }
}
