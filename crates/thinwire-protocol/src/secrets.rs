//! Process-local Telegram secret vault.
//!
//! Credential values never travel on [`crate::AdapterCommand`]. The UI and the
//! Telegram adapter share this map. OS keychain attach/flush lives in the
//! desktop crate and only persists [`TelegramSecretKey::PERSISTENT`] keys.

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;

/// Service name for the OS keychain / secret store.
pub const TELEGRAM_SECRET_SERVICE: &str = "thinwire";
/// Keychain account for the Telegram `api_id`.
pub const TELEGRAM_SECRET_API_ID: &str = "telegram.api_id";
/// Keychain account for the Telegram `api_hash`.
pub const TELEGRAM_SECRET_API_HASH: &str = "telegram.api_hash";
/// Keychain account for the TDLib session marker.
pub const TELEGRAM_SECRET_SESSION: &str = "telegram.session";
/// Memory-only account for the phone number (never flushed to the OS store).
pub const TELEGRAM_SECRET_PHONE: &str = "telegram.phone";
/// Memory-only account for the login code (never flushed to the OS store).
pub const TELEGRAM_SECRET_CODE: &str = "telegram.code";
/// Memory-only account for the 2FA password (never flushed to the OS store).
pub const TELEGRAM_SECRET_PASSWORD: &str = "telegram.password";

/// Telegram secrets the login screens and adapter share.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum TelegramSecretKey {
    ApiId,
    ApiHash,
    Phone,
    Code,
    Password,
    Session,
}

impl TelegramSecretKey {
    pub const ALL: [Self; 6] = [
        Self::ApiId,
        Self::ApiHash,
        Self::Phone,
        Self::Code,
        Self::Password,
        Self::Session,
    ];

    /// Keys that may be flushed to the OS keychain.
    pub const PERSISTENT: [Self; 3] = [Self::ApiId, Self::ApiHash, Self::Session];

    /// Keys that stay in the process map only.
    pub const EPHEMERAL: [Self; 3] = [Self::Phone, Self::Code, Self::Password];

    #[must_use]
    pub const fn account(self) -> &'static str {
        match self {
            Self::ApiId => TELEGRAM_SECRET_API_ID,
            Self::ApiHash => TELEGRAM_SECRET_API_HASH,
            Self::Phone => TELEGRAM_SECRET_PHONE,
            Self::Code => TELEGRAM_SECRET_CODE,
            Self::Password => TELEGRAM_SECRET_PASSWORD,
            Self::Session => TELEGRAM_SECRET_SESSION,
        }
    }

    #[must_use]
    pub const fn persist_to_os(self) -> bool {
        matches!(self, Self::ApiId | Self::ApiHash | Self::Session)
    }
}

impl fmt::Debug for TelegramSecretKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match *self {
            Self::ApiId => "ApiId",
            Self::ApiHash => "ApiHash",
            Self::Phone => "Phone",
            Self::Code => "Code",
            Self::Password => "Password",
            Self::Session => "Session",
        })
    }
}

/// Read/write Telegram secrets without logging values.
pub trait TelegramSecretVault: Send + Sync {
    fn get_secret(&self, key: TelegramSecretKey) -> Option<String>;
    fn set_secret(&self, key: TelegramSecretKey, value: &str);
}

/// In-memory vault used by tests and as the UI-side map.
pub struct MemorySecretVault {
    values: Mutex<HashMap<TelegramSecretKey, String>>,
}

impl MemorySecretVault {
    #[must_use]
    pub fn new() -> Self {
        Self {
            values: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for MemorySecretVault {
    fn default() -> Self {
        Self::new()
    }
}

impl TelegramSecretVault for MemorySecretVault {
    fn get_secret(&self, key: TelegramSecretKey) -> Option<String> {
        self.values
            .lock()
            .ok()
            .and_then(|values| values.get(&key).cloned())
    }

    fn set_secret(&self, key: TelegramSecretKey, value: &str) {
        let Ok(mut values) = self.values.lock() else {
            return;
        };
        let trimmed = value.trim();
        if trimmed.is_empty() {
            values.remove(&key);
        } else {
            values.insert(key, trimmed.to_string());
        }
    }
}

impl fmt::Debug for MemorySecretVault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemorySecretVault")
            .field("secrets", &"<redacted>")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persistent_keys_are_the_os_flush_set() {
        for key in TelegramSecretKey::PERSISTENT {
            assert!(key.persist_to_os());
        }
        for key in TelegramSecretKey::EPHEMERAL {
            assert!(!key.persist_to_os());
        }
        assert_eq!(TelegramSecretKey::ALL.len(), 6);
    }

    #[test]
    fn memory_vault_round_trip_redacts_debug() {
        let vault = MemorySecretVault::new();
        vault.set_secret(TelegramSecretKey::ApiId, "  11111  ");
        vault.set_secret(TelegramSecretKey::ApiHash, "super-secret-hash");
        vault.set_secret(TelegramSecretKey::Phone, "+15551234567");
        vault.set_secret(TelegramSecretKey::Code, "12345");
        vault.set_secret(TelegramSecretKey::Password, "2fa-secret");
        assert_eq!(
            vault.get_secret(TelegramSecretKey::ApiId).as_deref(),
            Some("11111")
        );
        vault.set_secret(TelegramSecretKey::Code, "  ");
        assert_eq!(vault.get_secret(TelegramSecretKey::Code), None);
        let debug = format!("{vault:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("11111"));
        assert!(!debug.contains("super-secret-hash"));
        assert!(!debug.contains("+15551234567"));
        assert!(!debug.contains("12345"));
        assert!(!debug.contains("2fa-secret"));
    }
}
