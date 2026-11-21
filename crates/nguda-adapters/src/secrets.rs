//! The secret store behind the [`SecretStore`] port: the OS keyring (macOS
//! Keychain, Linux Secret Service), through `keyring`'s `v1` stores (AD-13).

use keyring::{Entry, Error};
use nguda_core::{SecretStore, SecretStoreError};

/// Secrets in the OS keyring under one service name, keyed by account. Blocking:
/// call it off any async executor thread.
#[derive(Clone, Debug)]
pub struct KeyringSecretStore {
    service: String,
}

impl KeyringSecretStore {
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }

    fn entry(&self, key: &str) -> Result<Entry, SecretStoreError> {
        Entry::new(&self.service, key).map_err(|e| message(&e))
    }
}

impl SecretStore for KeyringSecretStore {
    fn get(&self, key: &str) -> Result<Option<String>, SecretStoreError> {
        match self.entry(key)?.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(Error::NoEntry) => Ok(None),
            Err(e) => Err(message(&e)),
        }
    }

    fn set(&self, key: &str, value: &str) -> Result<(), SecretStoreError> {
        self.entry(key)?
            .set_password(value)
            .map_err(|e| message(&e))
    }

    fn delete(&self, key: &str) -> Result<(), SecretStoreError> {
        match self.entry(key)?.delete_credential() {
            Ok(()) | Err(Error::NoEntry) => Ok(()),
            Err(e) => Err(message(&e)),
        }
    }
}

/// A fixed reason per error kind. Never the error's `Display`: some kinds carry the
/// stored bytes, which are the secret (AD-13, AD-14).
fn message(e: &Error) -> SecretStoreError {
    SecretStoreError::new(match e {
        Error::PlatformFailure(_) => "the keyring failed",
        Error::NoStorageAccess(_) => "the keyring is locked or not accessible",
        Error::NoEntry => "no such secret",
        Error::BadEncoding(_) | Error::BadDataFormat(..) => "the stored secret is unreadable",
        Error::NoDefaultStore => "no keyring is available",
        _ => "the keyring rejected the request",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_never_carry_the_stored_bytes() {
        let e = message(&Error::BadEncoding(b"secret-token".to_vec()));
        assert!(!e.message.contains("token"), "{e}");
    }

    /// Touches the real OS keyring, so it is opt-in.
    #[test]
    #[ignore = "uses the real OS keyring"]
    fn round_trips_through_the_real_keyring() {
        let store = KeyringSecretStore::new("nguda-test");
        store.set("probe", "value").unwrap();
        assert_eq!(store.get("probe").unwrap().as_deref(), Some("value"));
        store.delete("probe").unwrap();
        assert_eq!(store.get("probe").unwrap(), None);
        store.delete("probe").unwrap();
    }
}
