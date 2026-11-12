//! Secret storage for the Anthropic API key (S17).
//!
//! The key is the only secret; it lives in the system keyring via
//! [`KeyringStore`] under service [`SERVICE`], account [`ACCOUNT`]. Tests and
//! the `GENTLE_ENGINES_KEY_STORE=memory` path use [`MemoryStore`] so they never
//! touch the real keyring. The key is never logged, printed or placed in any
//! error or Debug output.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Keyring service name for all gentle-engines secrets.
pub const SERVICE: &str = "gentle-engines";
/// The single keyring account holding the Anthropic key.
pub const ACCOUNT: &str = "anthropic";

/// Required prefix of a valid Anthropic console API key.
pub const KEY_PREFIX: &str = "sk-ant-";

/// A place to keep the API key. Implementations must never log the value.
pub trait KeyStore: Send + Sync {
    /// Returns the stored key, or `None` if nothing is stored.
    fn load(&self) -> Result<Option<String>, String>;
    /// Stores (overwriting) the key.
    fn save(&self, key: &str) -> Result<(), String>;
    /// Removes the key. Deleting a missing entry is Ok.
    fn delete(&self) -> Result<(), String>;
}

/// Validates the surface format of an API key (prefix + non-trivial length).
/// Returns a message that never echoes the key itself.
pub fn validate_format(key: &str) -> Result<(), String> {
    let key = key.trim();
    if !key.starts_with(KEY_PREFIX) {
        return Err(format!("that does not look like an Anthropic key (expected a '{KEY_PREFIX}' prefix)"));
    }
    if key.len() < KEY_PREFIX.len() + 8 {
        return Err("that API key looks too short".to_string());
    }
    Ok(())
}

/// The system keyring (Secret Service via pure-Rust zbus).
#[derive(Default)]
pub struct KeyringStore;

impl KeyStore for KeyringStore {
    fn load(&self) -> Result<Option<String>, String> {
        let entry = keyring::Entry::new(SERVICE, ACCOUNT).map_err(|e| e.to_string())?;
        match entry.get_password() {
            Ok(key) => Ok(Some(key)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }
    fn save(&self, key: &str) -> Result<(), String> {
        let entry = keyring::Entry::new(SERVICE, ACCOUNT).map_err(|e| e.to_string())?;
        entry.set_password(key).map_err(|e| e.to_string())
    }
    fn delete(&self) -> Result<(), String> {
        let entry = keyring::Entry::new(SERVICE, ACCOUNT).map_err(|e| e.to_string())?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
}

/// In-memory store for tests and the `GENTLE_ENGINES_KEY_STORE=memory` path.
#[derive(Default, Clone)]
pub struct MemoryStore {
    inner: Arc<Mutex<HashMap<String, String>>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl KeyStore for MemoryStore {
    fn load(&self) -> Result<Option<String>, String> {
        Ok(self.inner.lock().unwrap().get(ACCOUNT).cloned())
    }
    fn save(&self, key: &str) -> Result<(), String> {
        self.inner.lock().unwrap().insert(ACCOUNT.to_string(), key.to_string());
        Ok(())
    }
    fn delete(&self) -> Result<(), String> {
        self.inner.lock().unwrap().remove(ACCOUNT);
        Ok(())
    }
}

/// Chooses the key store: in-memory when `GENTLE_ENGINES_KEY_STORE=memory`
/// (tests and the status smoke), else the system keyring.
pub fn select_store() -> Box<dyn KeyStore> {
    if std::env::var("GENTLE_ENGINES_KEY_STORE").as_deref() == Ok("memory") {
        Box::new(MemoryStore::new())
    } else {
        Box::new(KeyringStore)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_store_round_trips_and_deletes() {
        let s = MemoryStore::new();
        assert_eq!(s.load().unwrap(), None);
        s.save("sk-ant-abc12345").unwrap();
        assert_eq!(s.load().unwrap(), Some("sk-ant-abc12345".to_string()));
        s.delete().unwrap();
        assert_eq!(s.load().unwrap(), None);
        s.delete().unwrap(); // deleting a missing entry is Ok
    }

    #[test]
    fn validate_format_requires_prefix_and_length() {
        assert!(validate_format("sk-ant-api03-abcdefgh").is_ok());
        let err = validate_format("hunter2").unwrap_err();
        assert!(err.contains("sk-ant-"));
        // The error must never echo the rejected key.
        assert!(!err.contains("hunter2"));
        assert!(validate_format("sk-ant-").is_err());
    }
}
