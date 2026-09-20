//! OS secret store for Telegram `api_id`, `api_hash`, and session material.
//!
//! The UI thread only touches an in-memory map. OS keychain I/O runs on a
//! tokio `spawn_blocking` worker so egui never waits on keyutils / Keychain /
//! Credential Manager. Never log or persist these values in the git repo.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use keyring::Entry;
use thinwire_protocol::{
    TELEGRAM_SECRET_API_HASH, TELEGRAM_SECRET_API_ID, TELEGRAM_SECRET_SERVICE,
    TELEGRAM_SECRET_SESSION,
};
use tokio::runtime::Handle;

/// Set `THINWIRE_KEYRING=memory` to skip the OS keychain (CI / local headless).
pub const KEYRING_ENV: &str = "THINWIRE_KEYRING";
const KEYRING_MEMORY: &str = "memory";

/// Telegram secrets the login screens persist.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum SecretKey {
    ApiId,
    ApiHash,
    Session,
}

impl SecretKey {
    pub const ALL: [Self; 3] = [Self::ApiId, Self::ApiHash, Self::Session];

    #[must_use]
    pub const fn account(self) -> &'static str {
        match self {
            Self::ApiId => TELEGRAM_SECRET_API_ID,
            Self::ApiHash => TELEGRAM_SECRET_API_HASH,
            Self::Session => TELEGRAM_SECRET_SESSION,
        }
    }
}

impl fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ApiId => "ApiId",
            Self::ApiHash => "ApiHash",
            Self::Session => "Session",
        })
    }
}

/// Recoverable secret-store failure. Display text never includes secret values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretError {
    message: String,
}

impl SecretError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for SecretError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SecretError {}

/// Memory-first store. OS keychain attach/flush is worker-only.
pub struct SecretStore {
    memory: Mutex<HashMap<SecretKey, String>>,
    os: AtomicBool,
}

impl SecretStore {
    /// UI-safe constructor. Does not talk to the OS keychain.
    #[must_use]
    pub fn memory() -> Self {
        Self {
            memory: Mutex::new(HashMap::new()),
            os: AtomicBool::new(false),
        }
    }

    /// UI constructor plus a worker that attaches the OS keychain if available.
    #[must_use]
    pub fn for_ui(handle: &Handle) -> Arc<Self> {
        let store = Arc::new(Self::memory());
        store.spawn_os_attach(handle);
        store
    }

    /// Blocking attach used by tests. The app calls [`Self::for_ui`] instead.
    #[cfg(test)]
    #[must_use]
    pub fn open() -> Self {
        let store = Self::memory();
        store.attach_os_keychain();
        store
    }

    #[must_use]
    pub fn backend_name(&self) -> &'static str {
        if self.os.load(Ordering::Relaxed) {
            "os-keychain"
        } else {
            "memory"
        }
    }

    /// UI thread: update the in-memory map only.
    pub fn set(&self, key: SecretKey, value: &str) -> Result<(), SecretError> {
        let trimmed = value.trim();
        let mut map = self
            .memory
            .lock()
            .map_err(|_| SecretError::new("memory secret store is poisoned"))?;
        if trimmed.is_empty() {
            map.remove(&key);
        } else {
            map.insert(key, trimmed.to_string());
        }
        Ok(())
    }

    /// UI thread: read the in-memory map only.
    pub fn get(&self, key: SecretKey) -> Result<Option<String>, SecretError> {
        Ok(self
            .memory
            .lock()
            .map_err(|_| SecretError::new("memory secret store is poisoned"))?
            .get(&key)
            .cloned())
    }

    #[cfg(test)]
    pub fn delete(&self, key: SecretKey) -> Result<(), SecretError> {
        self.set(key, "")
    }

    pub fn spawn_os_attach(self: &Arc<Self>, handle: &Handle) {
        if memory_requested() {
            tracing::info!(
                "{KEYRING_ENV}={KEYRING_MEMORY}; Telegram secrets stay in memory this session"
            );
            return;
        }
        let store = Arc::clone(self);
        handle.spawn_blocking(move || store.attach_os_keychain());
    }

    pub fn spawn_os_flush(self: &Arc<Self>, handle: &Handle) {
        if !self.os.load(Ordering::Relaxed) {
            return;
        }
        let store = Arc::clone(self);
        handle.spawn_blocking(move || {
            if let Err(error) = store.flush_os() {
                tracing::warn!(error = %error, "OS keychain flush failed; secrets stay in memory");
            }
        });
    }

    /// Blocking. Worker / tests only.
    pub fn attach_os_keychain(&self) {
        if memory_requested() {
            return;
        }
        match probe_os() {
            Ok(()) => {
                self.os.store(true, Ordering::Relaxed);
                if let Err(error) = self.hydrate_from_os() {
                    tracing::warn!(
                        error = %error,
                        "OS keychain attached but hydrate failed; memory stays empty"
                    );
                } else {
                    tracing::info!("using the OS keychain for Telegram secrets");
                }
            }
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "OS keychain unavailable; Telegram secrets stay in memory this session and are not written to disk"
                );
            }
        }
    }

    fn hydrate_from_os(&self) -> Result<(), SecretError> {
        for key in SecretKey::ALL {
            if let Some(value) = os_get(key)? {
                self.set(key, &value)?;
            }
        }
        Ok(())
    }

    fn flush_os(&self) -> Result<(), SecretError> {
        for key in SecretKey::ALL {
            match self.get(key)? {
                Some(value) => os_set(key, &value)?,
                None => os_delete(key)?,
            }
        }
        Ok(())
    }
}

impl fmt::Debug for SecretStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretStore")
            .field("backend", &self.backend_name())
            .field("secrets", &"<redacted>")
            .finish()
    }
}

fn memory_requested() -> bool {
    std::env::var_os(KEYRING_ENV).is_some_and(|value| value == KEYRING_MEMORY)
}

fn probe_os() -> Result<(), SecretError> {
    let entry = os_entry(SecretKey::ApiId)?;
    match entry.get_password() {
        Ok(_) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(map_keyring_error(error)),
    }
}

fn os_entry(key: SecretKey) -> Result<Entry, SecretError> {
    Entry::new(TELEGRAM_SECRET_SERVICE, key.account()).map_err(map_keyring_error)
}

fn os_set(key: SecretKey, value: &str) -> Result<(), SecretError> {
    os_entry(key)?
        .set_password(value)
        .map_err(map_keyring_error)
}

fn os_get(key: SecretKey) -> Result<Option<String>, SecretError> {
    match os_entry(key)?.get_password() {
        Ok(value) => Ok(Some(value)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(map_keyring_error(error)),
    }
}

fn os_delete(key: SecretKey) -> Result<(), SecretError> {
    match os_entry(key)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(map_keyring_error(error)),
    }
}

fn map_keyring_error(error: keyring::Error) -> SecretError {
    let kind = match error {
        keyring::Error::NoEntry => "not found",
        keyring::Error::NoStorageAccess(_) => "no storage access",
        keyring::Error::PlatformFailure(_) => "platform failure",
        keyring::Error::TooLong(_, _) => "value too long",
        keyring::Error::Invalid(_, _) => "invalid attribute",
        keyring::Error::Ambiguous(_) => "ambiguous entry",
        other => {
            let _ = other;
            "keychain error"
        }
    };
    SecretError::new(format!("OS keychain: {kind}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_round_trip_does_not_leak_in_debug() {
        let store = SecretStore::memory();
        store.set(SecretKey::ApiId, "  12345  ").expect("set id");
        store
            .set(SecretKey::ApiHash, "super-secret-hash")
            .expect("set hash");
        store
            .set(SecretKey::Session, "super-secret-session")
            .expect("set session");
        assert_eq!(
            store.get(SecretKey::ApiId).expect("get id").as_deref(),
            Some("12345")
        );
        let debug = format!("{store:?}");
        assert!(debug.contains("memory"));
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("12345"));
        assert!(!debug.contains("super-secret-hash"));
        assert!(!debug.contains("super-secret-session"));
        store.delete(SecretKey::ApiHash).expect("delete hash");
        assert_eq!(store.get(SecretKey::ApiHash).expect("get hash"), None);
    }

    #[test]
    fn empty_value_deletes_key() {
        let store = SecretStore::memory();
        store.set(SecretKey::Session, "keep").expect("set");
        store.set(SecretKey::Session, "  ").expect("clear");
        assert_eq!(store.get(SecretKey::Session).expect("get"), None);
    }

    #[test]
    fn open_never_panics_without_a_desktop_keychain() {
        let store = SecretStore::open();
        assert!(matches!(store.backend_name(), "os-keychain" | "memory"));
        let debug = format!("{store:?}");
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn ui_set_does_not_require_os_keychain() {
        let store = SecretStore::memory();
        assert_eq!(store.backend_name(), "memory");
        store.set(SecretKey::ApiId, "11111").expect("memory set");
        assert_eq!(
            store.get(SecretKey::ApiId).expect("get").as_deref(),
            Some("11111")
        );
    }

    #[test]
    fn secret_error_display_has_no_value() {
        let err = SecretError::new("OS keychain: no storage access");
        assert!(!err.to_string().contains("api_hash"));
        assert!(!format!("{err:?}").contains("password"));
    }
}
