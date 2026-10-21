//! Random key for a helper session store.
//!
//! The helper creates the key and reads it from the OS keychain. The key
//! does not cross the pipe. It is not written to a file, a log line, a
//! command line, or an environment variable.
//!
//! One key opens every store of that helper. Signal uses `signal.store_key`.
//! WhatsApp uses `whatsapp.store_key`. A retained Signal file is the live
//! store after a rename, so it uses the Signal key.

use std::fmt;

use keyring_core::Entry;
use thinwire_protocol::TELEGRAM_SECRET_SERVICE;

use crate::secrets::{AttachError, OsBackend, memory_requested, probe_os};

/// Keychain account for the Signal session key. Service is `thinwire`.
pub const SIGNAL_STORE_KEY_ACCOUNT: &str = "signal.store_key";

/// Keychain account for the WhatsApp session key. Service is `thinwire`.
pub const WHATSAPP_STORE_KEY_ACCOUNT: &str = "whatsapp.store_key";

/// 32 random bytes. `Debug` and `Display` do not show the bytes.
pub struct StoreKey([u8; 32]);

impl StoreKey {
    /// Fill a new key from the system random source.
    ///
    /// Returns `None` when the random source fails. The caller does not keep
    /// a partial key.
    #[must_use]
    pub fn random() -> Option<Self> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).ok()?;
        Some(Self(bytes))
    }

    #[must_use]
    pub fn bytes(&self) -> [u8; 32] {
        self.0
    }

    #[must_use]
    fn to_hex(&self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut text = String::with_capacity(64);
        for byte in self.0 {
            text.push(HEX[(byte >> 4) as usize] as char);
            text.push(HEX[(byte & 0x0f) as usize] as char);
        }
        text
    }

    fn parse_hex(text: &str) -> Option<Self> {
        if text.len() != 64 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        let mut bytes = [0u8; 32];
        for (index, chunk) in text.as_bytes().chunks(2).enumerate() {
            let hi = hex_value(chunk[0])?;
            let lo = hex_value(chunk[1])?;
            bytes[index] = (hi << 4) | lo;
        }
        Some(Self(bytes))
    }
}

impl Clone for StoreKey {
    fn clone(&self) -> Self {
        Self(self.0)
    }
}

impl Drop for StoreKey {
    fn drop(&mut self) {
        self.0 = [0; 32];
    }
}

impl fmt::Debug for StoreKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("StoreKey([redacted])")
    }
}

impl fmt::Display for StoreKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Whether this process may keep a session key in the OS keychain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreKeyPolicy {
    /// The keychain does not keep a value across a restart.
    NotSaved,
    /// Secret Service, macOS Keychain, or Windows Credential Manager.
    UseOs,
}

/// `memory` is `THINWIRE_KEYRING=memory`. `backend` is the store from a
/// probe. `None` means no OS store is present.
#[must_use]
pub const fn store_key_policy(memory: bool, backend: Option<OsBackend>) -> StoreKeyPolicy {
    if memory {
        return StoreKeyPolicy::NotSaved;
    }
    match backend {
        Some(OsBackend::SecretService | OsBackend::Native) => StoreKeyPolicy::UseOs,
        Some(OsBackend::KernelKeyring) | None => StoreKeyPolicy::NotSaved,
    }
}

/// Result of a keychain read. `Missing` means the entry is not there.
/// `Unavailable` means the read failed or the stored text is not a key.
/// A failed read does not overwrite the entry.
#[derive(Debug)]
pub enum LoadStoreKey {
    Found(StoreKey),
    Missing,
    NotSaved,
    Unavailable,
}

/// Why a new key was not written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateStoreKeyError {
    NotSaved,
    Unavailable,
}

/// Read the session key for `account`.
///
/// A memory keychain and kernel keyutils return [`LoadStoreKey::NotSaved`].
/// This function does not create a key and does not write a store.
#[must_use]
pub fn load_store_key(account: &str) -> LoadStoreKey {
    match decide() {
        Ok(StoreKeyPolicy::NotSaved) => LoadStoreKey::NotSaved,
        Err(()) => LoadStoreKey::Unavailable,
        Ok(StoreKeyPolicy::UseOs) => read_os_key(account),
    }
}

/// Create a random key and save it under `account`.
///
/// # Errors
///
/// [`CreateStoreKeyError::NotSaved`] when the keychain cannot keep the key.
/// Nothing is written. [`CreateStoreKeyError::Unavailable`] when the save fails.
pub fn create_store_key(account: &str) -> Result<StoreKey, CreateStoreKeyError> {
    match decide() {
        Ok(StoreKeyPolicy::NotSaved) => return Err(CreateStoreKeyError::NotSaved),
        Err(()) => return Err(CreateStoreKeyError::Unavailable),
        Ok(StoreKeyPolicy::UseOs) => {}
    }
    let key = StoreKey::random().ok_or(CreateStoreKeyError::Unavailable)?;
    let mut hex = key.to_hex();
    let entry = match Entry::new(TELEGRAM_SECRET_SERVICE, account) {
        Ok(entry) => entry,
        Err(_) => {
            wipe(&mut hex);
            return Err(CreateStoreKeyError::Unavailable);
        }
    };
    let saved = entry.set_password(&hex);
    wipe(&mut hex);
    saved
        .map(|()| key)
        .map_err(|_| CreateStoreKeyError::Unavailable)
}

fn decide() -> Result<StoreKeyPolicy, ()> {
    if memory_requested() {
        return Ok(store_key_policy(true, None));
    }
    match probe_os() {
        Ok(backend) => Ok(store_key_policy(false, Some(backend))),
        Err(AttachError::Absent(_)) => Ok(store_key_policy(false, None)),
        Err(AttachError::ReadFailed(_)) => Err(()),
    }
}

fn read_os_key(account: &str) -> LoadStoreKey {
    let entry = match Entry::new(TELEGRAM_SECRET_SERVICE, account) {
        Ok(entry) => entry,
        Err(_) => return LoadStoreKey::Unavailable,
    };
    match entry.get_password() {
        Ok(mut text) => {
            let parsed = StoreKey::parse_hex(&text);
            wipe(&mut text);
            match parsed {
                Some(key) => LoadStoreKey::Found(key),
                None => LoadStoreKey::Unavailable,
            }
        }
        Err(keyring_core::Error::NoEntry) => LoadStoreKey::Missing,
        Err(_) => LoadStoreKey::Unavailable,
    }
}

fn wipe(text: &mut String) {
    let width = text.len();
    text.clear();
    text.extend(std::iter::repeat_n('\0', width));
    text.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_memory_keychain_does_not_save_the_session_key() {
        assert_eq!(
            store_key_policy(true, Some(OsBackend::SecretService)),
            StoreKeyPolicy::NotSaved
        );
        assert_eq!(
            store_key_policy(true, Some(OsBackend::Native)),
            StoreKeyPolicy::NotSaved
        );
        assert_eq!(
            store_key_policy(true, Some(OsBackend::KernelKeyring)),
            StoreKeyPolicy::NotSaved
        );
    }

    #[test]
    fn keyutils_and_a_missing_store_do_not_save_the_session_key() {
        assert_eq!(
            store_key_policy(false, Some(OsBackend::KernelKeyring)),
            StoreKeyPolicy::NotSaved
        );
        assert_eq!(store_key_policy(false, None), StoreKeyPolicy::NotSaved);
    }

    #[test]
    fn secret_service_and_the_native_store_save_the_session_key() {
        assert_eq!(
            store_key_policy(false, Some(OsBackend::SecretService)),
            StoreKeyPolicy::UseOs
        );
        assert_eq!(
            store_key_policy(false, Some(OsBackend::Native)),
            StoreKeyPolicy::UseOs
        );
    }

    #[test]
    fn debug_and_display_hide_the_key_bytes() {
        let key = StoreKey([0xab; 32]);
        let text = format!("{key:?} {key}");
        assert!(text.contains("redacted"));
        assert!(!text.to_ascii_lowercase().contains("ab"));
    }

    #[test]
    fn hex_round_trip_keeps_all_32_bytes() {
        let key = StoreKey([0x3c; 32]);
        let hex = key.to_hex();
        let parsed = StoreKey::parse_hex(&hex).expect("hex");
        assert_eq!(parsed.bytes(), key.bytes());
        assert!(StoreKey::parse_hex("abcd").is_none());
        assert!(StoreKey::parse_hex(&"zz".repeat(32)).is_none());
    }
}
