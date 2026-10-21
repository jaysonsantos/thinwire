// SPDX-License-Identifier: AGPL-3.0-only
//! Encrypt the Signal session stores inside this helper.
//!
//! One keychain key opens the live store, the pending link file, and every
//! retained store. The key does not cross the pipe. A test build of this
//! library uses a fixed key. The helper program does not: that program reads
//! the OS keychain and refuses a keychain that cannot save.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use super::path::{PENDING_FILE, RETAINED_DIR, SQLITE_FILE};

#[cfg(not(test))]
use thinwire_core::{
    CreateStoreKeyError, LoadStoreKey, SIGNAL_STORE_KEY_ACCOUNT, StoreKeyAction, create_store_key,
    load_store_key, store_key_action,
};
use thinwire_protocol::{
    MigrateOptions, PrepareError, is_encrypted_store, move_stores_aside, prepare_file,
    store_sibling,
};

/// Why the stores were not opened. The app maps this to the account row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProtectFail {
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

static SESSION_KEY: OnceLock<[u8; 32]> = OnceLock::new();

/// Copy every existing store to SQLCipher, then keep the key for later opens.
///
/// # Errors
///
/// The string is the account-row sentence. A failure does not delete a store.
pub(super) fn protect_session(dir: &Path) -> Result<(), ProtectFail> {
    let files = store_files(dir);
    // A bad file under retained/ is not a reason to treat the live store as encrypted.
    let encrypted_live = files
        .iter()
        .any(|path| !is_retained(dir, path) && is_encrypted_store(path));
    let key = resolve_key(encrypted_live, dir)?;
    let _ = SESSION_KEY.set(key);
    for path in &files {
        match prepare_file(path, &key, MigrateOptions::default()) {
            Ok(_) => {}
            Err(PrepareError::WrongKey | PrepareError::Unreadable) if is_retained(dir, path) => {
                // The path holds the account id. The log line does not.
                tracing::warn!("{RETAINED_LEFT}");
            }
            Err(PrepareError::WrongKey) => return Err(move_aside(dir)),
            Err(PrepareError::Unreadable | PrepareError::CheckFailed | PrepareError::Io) => {
                return Err(ProtectFail::EncryptFailed);
            }
        }
    }
    Ok(())
}

/// Log text for a retained store that stays. It has no account id and no path.
const RETAINED_LEFT: &str = "a retained Signal store was left in place";

/// sqlx options that set the raw key before any other statement.
///
/// # Errors
///
/// Returns an error when the key is not available. The options are not logged.
pub(super) fn connect_options(
    url: &str,
    create: bool,
    read_only: bool,
) -> Result<presage_store_sqlite::SqliteConnectOptions, ()> {
    let key = session_key()?;
    let literal = thinwire_protocol::raw_key_literal(&key);
    Ok(url
        .parse::<presage_store_sqlite::SqliteConnectOptions>()
        .map_err(|_| ())?
        .create_if_missing(create)
        .read_only(read_only)
        .foreign_keys(true)
        .busy_timeout(std::time::Duration::from_secs(2))
        .pragma("key", literal))
}

/// The raw-key SQL fragment. The caller does not log it.
pub(super) fn raw_key_sql() -> Result<String, ()> {
    session_key().map(|key| thinwire_protocol::raw_key_literal(&key))
}

fn session_key() -> Result<[u8; 32], ()> {
    if let Some(key) = SESSION_KEY.get() {
        return Ok(*key);
    }
    remember_key()
}

#[cfg(test)]
fn remember_key() -> Result<[u8; 32], ()> {
    let key = test_key();
    let _ = SESSION_KEY.set(key);
    Ok(key)
}

#[cfg(not(test))]
fn remember_key() -> Result<[u8; 32], ()> {
    match load_store_key(SIGNAL_STORE_KEY_ACCOUNT) {
        LoadStoreKey::Found(key) => {
            let bytes = key.bytes();
            let _ = SESSION_KEY.set(bytes);
            Ok(bytes)
        }
        LoadStoreKey::Missing | LoadStoreKey::NotSaved | LoadStoreKey::Unavailable => Err(()),
    }
}

#[cfg(test)]
fn resolve_key(_encrypted_present: bool, _dir: &Path) -> Result<[u8; 32], ProtectFail> {
    Ok(test_key())
}

#[cfg(test)]
fn test_key() -> [u8; 32] {
    [0x5a; 32]
}

#[cfg(not(test))]
fn resolve_key(encrypted_present: bool, dir: &Path) -> Result<[u8; 32], ProtectFail> {
    let loaded = load_store_key(SIGNAL_STORE_KEY_ACCOUNT);
    match store_key_action(&loaded, encrypted_present) {
        StoreKeyAction::Open => match loaded {
            LoadStoreKey::Found(key) => Ok(key.bytes()),
            LoadStoreKey::Missing | LoadStoreKey::NotSaved | LoadStoreKey::Unavailable => {
                Err(ProtectFail::KeychainDenied)
            }
        },
        StoreKeyAction::Denied => Err(ProtectFail::KeychainDenied),
        StoreKeyAction::NotSaved => Err(ProtectFail::NotSaved),
        StoreKeyAction::MoveAside => Err(move_aside(dir)),
        StoreKeyAction::Create => match create_store_key(SIGNAL_STORE_KEY_ACCOUNT) {
            Ok(key) => Ok(key.bytes()),
            Err(CreateStoreKeyError::NotSaved) => Err(ProtectFail::NotSaved),
            Err(CreateStoreKeyError::Unavailable) => Err(ProtectFail::KeychainDenied),
        },
    }
}

fn store_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    push_store(dir, SQLITE_FILE, &mut files);
    push_store(dir, PENDING_FILE, &mut files);
    let retained = dir.join(RETAINED_DIR);
    let Ok(entries) = std::fs::read_dir(&retained) else {
        return files;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            push_store(&path, SQLITE_FILE, &mut files);
        }
    }
    files
}

fn push_store(dir: &Path, file_name: &str, out: &mut Vec<PathBuf>) {
    let path = dir.join(file_name);
    let enc_new = sibling(&path, ".enc-new");
    let plain_old = sibling(&path, ".plain-old");
    if path.exists() || enc_new.exists() || plain_old.exists() {
        out.push(path);
    }
}

fn is_retained(dir: &Path, path: &Path) -> bool {
    path.starts_with(dir.join(RETAINED_DIR))
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    store_sibling(path, suffix)
}

fn move_aside(dir: &Path) -> ProtectFail {
    if move_stores_aside(dir).is_ok() {
        ProtectFail::WrongKey
    } else {
        ProtectFail::NotMoved
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thinwire_protocol::is_plaintext_sqlite;

    #[test]
    fn a_denied_read_is_not_a_wrong_key() {
        assert_ne!(ProtectFail::KeychainDenied, ProtectFail::WrongKey);
        assert_ne!(ProtectFail::NotSaved, ProtectFail::WrongKey);
        assert_ne!(ProtectFail::KeychainDenied, ProtectFail::NotSaved);
        assert_ne!(ProtectFail::EncryptFailed, ProtectFail::KeychainDenied);
        assert_ne!(ProtectFail::NotMoved, ProtectFail::WrongKey);
    }

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "thinwire-signal-cipher-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    fn write_plain(path: &Path, rows: i64) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("parent");
        }
        let conn = rusqlite::Connection::open(path).expect("plain");
        conn.execute_batch("CREATE TABLE items (id INTEGER PRIMARY KEY);")
            .expect("schema");
        for id in 1..=rows {
            conn.execute("INSERT INTO items (id) VALUES (?1)", [id])
                .expect("row");
        }
    }

    fn item_count(path: &Path) -> i64 {
        let conn = rusqlite::Connection::open(path).expect("open");
        let mut sql = format!(
            "PRAGMA key = {};",
            thinwire_protocol::raw_key_literal(&test_key())
        );
        conn.execute_batch(&sql).expect("key");
        sql.clear();
        conn.query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
            .expect("count")
    }

    #[test]
    fn protect_encrypts_the_live_pending_and_retained_stores() {
        let dir = temp_dir("retained");
        let live = dir.join(SQLITE_FILE);
        let pending = dir.join(PENDING_FILE);
        let unstored = dir.join(RETAINED_DIR).join("unstored").join(SQLITE_FILE);
        let aci = dir
            .join(RETAINED_DIR)
            .join("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee")
            .join(SQLITE_FILE);
        let aci_again = dir
            .join(RETAINED_DIR)
            .join("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee-2")
            .join(SQLITE_FILE);
        write_plain(&live, 3);
        write_plain(&pending, 1);
        write_plain(&unstored, 2);
        write_plain(&aci, 4);
        write_plain(&aci_again, 5);
        protect_session(&dir).expect("protect");
        for (path, rows) in [
            (&live, 3),
            (&pending, 1),
            (&unstored, 2),
            (&aci, 4),
            (&aci_again, 5),
        ] {
            assert!(!is_plaintext_sqlite(path), "{}", path.display());
            assert_eq!(item_count(path), rows);
            assert!(!sibling(path, ".enc-new").exists());
            assert!(!sibling(path, ".plain-old").exists());
            assert!(!sibling(path, "-wal").exists());
            assert!(!sibling(path, "-shm").exists());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_wrong_key_moves_the_session_folder_aside() {
        let parent = temp_dir("aside");
        let dir = parent.join("signal");
        std::fs::create_dir_all(&dir).expect("dir");
        let live = dir.join(SQLITE_FILE);
        write_plain(&live, 2);
        let other = [0x11u8; 32];
        prepare_file(&live, &other, MigrateOptions::default()).expect("encrypt");
        let error = protect_session(&dir).expect_err("wrong key");
        assert_eq!(error, ProtectFail::WrongKey);
        assert!(dir.is_dir(), "the session directory stays");
        assert!(
            !live.exists(),
            "the store file leaves the session directory"
        );
        let moved = std::fs::read_dir(&parent)
            .expect("parent")
            .flatten()
            .find(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("signal.stale-")
            })
            .expect("stale folder");
        let kept = moved.path().join(SQLITE_FILE);
        assert!(kept.is_file());
        assert!(!is_plaintext_sqlite(&kept));
        let _ = std::fs::remove_dir_all(&parent);
    }

    #[test]
    fn a_failed_move_aside_is_not_a_wrong_key() {
        let parent = temp_dir("stuck");
        let dir = parent.join("signal");
        std::fs::create_dir_all(&dir).expect("dir");
        let live = dir.join(SQLITE_FILE);
        write_plain(&live, 2);
        let other = [0x11u8; 32];
        prepare_file(&live, &other, MigrateOptions::default()).expect("encrypt");
        let before = std::fs::read(&live).expect("bytes");
        block_stale_names(&parent, "signal");
        let error = protect_session(&dir).expect_err("move failed");
        assert_eq!(error, ProtectFail::NotMoved);
        assert_ne!(error, ProtectFail::WrongKey);
        assert_eq!(std::fs::read(&live).expect("left in place"), before);
        assert!(dir.join(SQLITE_FILE).is_file());
        let _ = std::fs::remove_dir_all(&parent);
    }

    /// Every stale name the move would pick, for this second and the next few.
    fn block_stale_names(parent: &Path, dir_name: &str) {
        let start = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_secs();
        for stamp in start..=start + 8 {
            let base = format!("{dir_name}.stale-{stamp}");
            for suffix in [String::new()]
                .into_iter()
                .chain((2..1_000).map(|index| format!("-{index}")))
            {
                std::fs::write(parent.join(format!("{base}{suffix}")), b"taken").expect("block");
            }
        }
    }

    #[test]
    fn a_bad_retained_store_does_not_move_the_live_session() {
        let parent = temp_dir("retained-bad");
        let dir = parent.join("signal");
        let live = dir.join(SQLITE_FILE);
        write_plain(&live, 2);
        let aci = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let retained = dir.join(RETAINED_DIR).join(aci).join(SQLITE_FILE);
        std::fs::create_dir_all(retained.parent().expect("parent")).expect("retained");
        let garbage = b"not a database";
        std::fs::write(&retained, garbage).expect("garbage");
        assert!(!RETAINED_LEFT.contains(aci));
        assert!(!RETAINED_LEFT.contains('/'));
        protect_session(&dir).expect("live store still opens");
        assert!(dir.is_dir());
        assert!(live.is_file());
        assert!(!is_plaintext_sqlite(&live));
        assert_eq!(item_count(&live), 2);
        assert_eq!(std::fs::read(&retained).expect("untouched"), garbage);
        let stale = std::fs::read_dir(&parent)
            .expect("parent")
            .flatten()
            .any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("signal.stale-")
            });
        assert!(!stale, "a bad retained file does not move the session");
        let _ = std::fs::remove_dir_all(&parent);
    }

    #[test]
    fn a_second_helper_cannot_lock_the_session_after_a_wrong_key() {
        let parent = temp_dir("lock");
        let dir = parent.join("signal");
        std::fs::create_dir_all(&dir).expect("dir");
        let live = dir.join(SQLITE_FILE);
        write_plain(&live, 1);
        let other = [0x11u8; 32];
        thinwire_protocol::prepare_file(&live, &other, MigrateOptions::default()).expect("encrypt");
        let held = super::super::path::lock_session_at(&dir).expect("first helper");
        let error = protect_session(&dir).expect_err("wrong key");
        assert_eq!(error, ProtectFail::WrongKey);
        assert!(dir.join("helper.lock").is_file());
        assert!(!dir.join(SQLITE_FILE).exists());
        let second = super::super::path::lock_session_at(&dir);
        assert_eq!(
            second.expect_err("second helper"),
            super::super::path::SessionLockError::InUse
        );
        drop(held);
        let _ = std::fs::remove_dir_all(&parent);
    }
}
