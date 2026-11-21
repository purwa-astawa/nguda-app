//! The secret store port: small secrets such as the Google refresh token, kept out
//! of SQLite, snapshots, and logs (AD-13).

use std::fmt;

/// A secret store failure. The message is the backend's error or a fixed reason,
/// never the secret (AD-13, AD-14).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecretStoreError {
    pub message: String,
}

impl SecretStoreError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for SecretStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SecretStoreError {}

/// Secrets keyed by name. Blocking: callers run it off any async executor thread.
pub trait SecretStore: Send + Sync + 'static {
    /// The secret stored under `key`, or `None` when there is none.
    fn get(&self, key: &str) -> Result<Option<String>, SecretStoreError>;

    /// Stores `value` under `key`, replacing any earlier one.
    fn set(&self, key: &str, value: &str) -> Result<(), SecretStoreError>;

    /// Deletes the secret under `key`. Deleting a missing one is not an error.
    fn delete(&self, key: &str) -> Result<(), SecretStoreError>;
}
