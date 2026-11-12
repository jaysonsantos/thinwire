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

/// What the helper does with a keychain read.
///
/// A denied read never moves the store. A missing key moves the folder
/// aside only when an encrypted file is already there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreKeyAction {
    /// Open with the key that was found.
    Open,
    /// The read failed. Do not move the store and do not create a key.
    Denied,
    /// The keychain cannot keep a key. Do not move the store.
    NotSaved,
    /// No key, and an encrypted file is present. Move the folder aside.
    MoveAside,
    /// No key and no encrypted file. Create a key, then encrypt.
    Create,
}

/// Choose the next step from a keychain read.
///
/// `encrypted_present` is true when a store file is already encrypted.
#[must_use]
pub const fn store_key_action(loaded: &LoadStoreKey, encrypted_present: bool) -> StoreKeyAction {
    match loaded {
        LoadStoreKey::Found(_) => StoreKeyAction::Open,
        LoadStoreKey::Unavailable => StoreKeyAction::Denied,
        LoadStoreKey::NotSaved => StoreKeyAction::NotSaved,
        LoadStoreKey::Missing if encrypted_present => StoreKeyAction::MoveAside,
        LoadStoreKey::Missing => StoreKeyAction::Create,
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

/// Why the stores of a helper were not opened. The app maps this to the
/// account row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreProtectFail {
    /// The keychain refused the read. The store was not moved.
    KeychainDenied,
    /// The encrypted store did not open. The store files were moved aside.
    /// `helper.lock` stays in the session folder.
    WrongKey,
    /// The store files could not be moved aside. They were left in place.
    /// Nothing was deleted.
    NotMoved,
    /// The keychain cannot keep a key. The store was not moved.
    NotSaved,
    /// The export or the row check failed. The plaintext store stays.
    EncryptFailed,
}

impl StoreProtectFail {
    /// The result of a move of the store files: `moved` is true when every
    /// file left the session folder.
    #[must_use]
    pub const fn after_move(moved: bool) -> Self {
        if moved {
            Self::WrongKey
        } else {
            Self::NotMoved
        }
    }
}

/// The key that opens the stores of one helper, from the keychain entry
/// `account`. `encrypted_present` is true when a store file is already
/// encrypted. `move_aside` moves the store files out of the session folder
/// and says if every file moved. It runs only for a key that is missing
/// while an encrypted file is there.
///
/// # Errors
///
/// [`StoreProtectFail`] for a keychain that refuses the read or cannot keep
/// a key, and for a missing key of an encrypted store. It is never
/// [`StoreProtectFail::EncryptFailed`].
pub fn resolve_store_key(
    account: &str,
    encrypted_present: bool,
    move_aside: impl FnOnce() -> bool,
) -> Result<StoreKey, StoreProtectFail> {
    resolve_with(
        load_store_key(account),
        encrypted_present,
        || create_store_key(account),
        move_aside,
    )
}

/// [`resolve_store_key`] with the keychain read and the key creation as
/// parameters.
fn resolve_with(
    loaded: LoadStoreKey,
    encrypted_present: bool,
    create: impl FnOnce() -> Result<StoreKey, CreateStoreKeyError>,
    move_aside: impl FnOnce() -> bool,
) -> Result<StoreKey, StoreProtectFail> {
    match store_key_action(&loaded, encrypted_present) {
        StoreKeyAction::Open => match loaded {
            LoadStoreKey::Found(key) => Ok(key),
            LoadStoreKey::Missing | LoadStoreKey::NotSaved | LoadStoreKey::Unavailable => {
                Err(StoreProtectFail::KeychainDenied)
            }
        },
        StoreKeyAction::Denied => Err(StoreProtectFail::KeychainDenied),
        StoreKeyAction::NotSaved => Err(StoreProtectFail::NotSaved),
        StoreKeyAction::MoveAside => Err(StoreProtectFail::after_move(move_aside())),
        StoreKeyAction::Create => create().map_err(|error| match error {
            CreateStoreKeyError::NotSaved => StoreProtectFail::NotSaved,
            CreateStoreKeyError::Unavailable => StoreProtectFail::KeychainDenied,
        }),
    }
}

/// The store key of one helper process, read from the keychain one time.
/// Each later open of a store uses the same bytes.
pub struct SessionKey(std::sync::OnceLock<[u8; 32]>);

impl SessionKey {
    #[must_use]
    pub const fn new() -> Self {
        Self(std::sync::OnceLock::new())
    }

    /// The key of the keychain entry `account`. `None` when the entry has
    /// no key or the read fails: the next call reads again.
    #[must_use]
    pub fn saved(&self, account: &str) -> Option<[u8; 32]> {
        if let Some(key) = self.0.get() {
            return Some(*key);
        }
        match load_store_key(account) {
            LoadStoreKey::Found(key) => Some(self.keep(key.bytes())),
            LoadStoreKey::Missing | LoadStoreKey::NotSaved | LoadStoreKey::Unavailable => None,
        }
    }

    /// Keep `key` for this process, with no keychain read. The first key
    /// stays. A test build of a helper uses a fixed key.
    pub fn keep(&self, key: [u8; 32]) -> [u8; 32] {
        *self.0.get_or_init(|| key)
    }
}

impl Default for SessionKey {
    fn default() -> Self {
        Self::new()
    }
}

/// The bytes do not show.
impl fmt::Debug for SessionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionKey")
            .field("read", &self.0.get().is_some())
            .finish()
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

    fn resolved(
        loaded: LoadStoreKey,
        encrypted_present: bool,
        moved: bool,
    ) -> (Result<[u8; 32], StoreProtectFail>, bool, bool) {
        let created = std::cell::Cell::new(false);
        let asked_move = std::cell::Cell::new(false);
        let key = resolve_with(
            loaded,
            encrypted_present,
            || {
                created.set(true);
                Ok(StoreKey([7; 32]))
            },
            || {
                asked_move.set(true);
                moved
            },
        );
        (key.map(|key| key.bytes()), created.get(), asked_move.get())
    }

    #[test]
    fn a_found_key_opens_the_store_and_changes_nothing() {
        for encrypted in [false, true] {
            let (key, created, moved) =
                resolved(LoadStoreKey::Found(StoreKey([3; 32])), encrypted, true);
            assert_eq!(key, Ok([3; 32]));
            assert!(!created && !moved);
        }
    }

    #[test]
    fn a_denied_read_and_a_keychain_that_cannot_save_never_move_the_store() {
        for encrypted in [false, true] {
            let (key, created, moved) = resolved(LoadStoreKey::Unavailable, encrypted, true);
            assert_eq!(key, Err(StoreProtectFail::KeychainDenied));
            assert!(!created && !moved);
            let (key, created, moved) = resolved(LoadStoreKey::NotSaved, encrypted, true);
            assert_eq!(key, Err(StoreProtectFail::NotSaved));
            assert!(!created && !moved);
        }
    }

    #[test]
    fn a_missing_key_of_an_encrypted_store_moves_it_aside_and_creates_no_key() {
        let (key, created, moved) = resolved(LoadStoreKey::Missing, true, true);
        assert_eq!(key, Err(StoreProtectFail::WrongKey));
        assert!(!created && moved);
        let (key, created, moved) = resolved(LoadStoreKey::Missing, true, false);
        assert_eq!(key, Err(StoreProtectFail::NotMoved));
        assert!(!created && moved);
    }

    #[test]
    fn a_missing_key_with_no_encrypted_store_creates_a_key() {
        let (key, created, moved) = resolved(LoadStoreKey::Missing, false, true);
        assert_eq!(key, Ok([7; 32]));
        assert!(created && !moved);
        let refused = |error| {
            resolve_with(LoadStoreKey::Missing, false, || Err(error), || true)
                .map(|key| key.bytes())
        };
        assert_eq!(
            refused(CreateStoreKeyError::NotSaved),
            Err(StoreProtectFail::NotSaved)
        );
        assert_eq!(
            refused(CreateStoreKeyError::Unavailable),
            Err(StoreProtectFail::KeychainDenied)
        );
    }

    #[test]
    fn the_session_key_keeps_its_first_value() {
        let key = SessionKey::new();
        assert_eq!(key.keep([1; 32]), [1; 32]);
        assert_eq!(key.keep([2; 32]), [1; 32]);
        assert_eq!(key.saved("an entry that is not read"), Some([1; 32]));
        assert_eq!(format!("{key:?}"), "SessionKey { read: true }");
    }

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
    fn a_denied_read_does_not_move_the_store() {
        let denied = LoadStoreKey::Unavailable;
        assert_eq!(store_key_action(&denied, true), StoreKeyAction::Denied);
        assert_eq!(store_key_action(&denied, false), StoreKeyAction::Denied);
        let unsaved = LoadStoreKey::NotSaved;
        assert_eq!(store_key_action(&unsaved, true), StoreKeyAction::NotSaved);
        assert_eq!(
            store_key_action(&LoadStoreKey::Missing, true),
            StoreKeyAction::MoveAside
        );
        assert_eq!(
            store_key_action(&LoadStoreKey::Missing, false),
            StoreKeyAction::Create
        );
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
