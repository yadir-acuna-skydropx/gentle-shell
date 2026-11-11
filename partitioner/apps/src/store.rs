//! Secret storage for Slack tokens.
//!
//! The access token (and the refresh token + expiry, when Slack rotation is on)
//! are the only secrets; they live in the system keyring via [`KeyringStore`].
//! Tests inject [`MemoryStore`] so they never touch the real keyring.
//!
//! Storage choice: a **single keyring account** (`slack`) under service
//! `gentle-apps`, holding a small JSON blob. One workspace per machine is enough
//! for the MVP; the non-secret metadata (team name, user id) lives in
//! `apps.toml`, never the token.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

/// Keyring service name shared by all gentle-apps secrets.
pub const SERVICE: &str = "gentle-apps";
/// The single keyring account used for Slack.
pub const SLACK_ACCOUNT: &str = "slack";

/// The secret material persisted for a Slack login. Never printed or logged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredToken {
    /// The user token (`xoxp-…`).
    pub access_token: String,
    /// Present only when token rotation is enabled on the app.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Unix epoch seconds when `access_token` expires (rotation only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
}

/// A place to keep [`StoredToken`]s keyed by an account string.
pub trait TokenStore: Send + Sync {
    /// Returns the token for `account`, or `None` if nothing is stored.
    fn load(&self, account: &str) -> Result<Option<StoredToken>, String>;
    /// Stores (overwriting) the token for `account`.
    fn save(&self, account: &str, token: &StoredToken) -> Result<(), String>;
    /// Removes the token for `account`. Deleting a missing entry is Ok.
    fn delete(&self, account: &str) -> Result<(), String>;
}

/// The system keyring (Secret Service via pure-Rust zbus).
#[derive(Default)]
pub struct KeyringStore;

impl TokenStore for KeyringStore {
    fn load(&self, account: &str) -> Result<Option<StoredToken>, String> {
        let entry = keyring::Entry::new(SERVICE, account).map_err(|e| e.to_string())?;
        match entry.get_password() {
            Ok(json) => {
                let token: StoredToken = serde_json::from_str(&json)
                    .map_err(|e| format!("corrupt keyring entry: {e}"))?;
                Ok(Some(token))
            }
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }

    fn save(&self, account: &str, token: &StoredToken) -> Result<(), String> {
        let entry = keyring::Entry::new(SERVICE, account).map_err(|e| e.to_string())?;
        let json = serde_json::to_string(token).map_err(|e| e.to_string())?;
        entry.set_password(&json).map_err(|e| e.to_string())
    }

    fn delete(&self, account: &str) -> Result<(), String> {
        let entry = keyring::Entry::new(SERVICE, account).map_err(|e| e.to_string())?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
}

/// In-memory store for tests and for the `GENTLE_APPS_TOKEN_STORE=memory`
/// binary path (never persists; empty on each process start).
#[derive(Default, Clone)]
pub struct MemoryStore {
    inner: Arc<Mutex<HashMap<String, StoredToken>>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl TokenStore for MemoryStore {
    fn load(&self, account: &str) -> Result<Option<StoredToken>, String> {
        Ok(self.inner.lock().unwrap().get(account).cloned())
    }
    fn save(&self, account: &str, token: &StoredToken) -> Result<(), String> {
        self.inner.lock().unwrap().insert(account.to_string(), token.clone());
        Ok(())
    }
    fn delete(&self, account: &str) -> Result<(), String> {
        self.inner.lock().unwrap().remove(account);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_store_round_trips_and_deletes() {
        let store = MemoryStore::new();
        assert_eq!(store.load("slack").unwrap(), None);
        let token = StoredToken {
            access_token: "xoxp-secret".into(),
            refresh_token: Some("xoxe-1-refresh".into()),
            expires_at: Some(123),
        };
        store.save("slack", &token).unwrap();
        assert_eq!(store.load("slack").unwrap(), Some(token));
        store.delete("slack").unwrap();
        assert_eq!(store.load("slack").unwrap(), None);
        // Deleting a missing entry is not an error.
        store.delete("slack").unwrap();
    }

    #[test]
    fn stored_token_omits_absent_optional_fields() {
        let token = StoredToken {
            access_token: "xoxp-1".into(),
            refresh_token: None,
            expires_at: None,
        };
        let json = serde_json::to_string(&token).unwrap();
        assert_eq!(json, r#"{"access_token":"xoxp-1"}"#);
    }
}
