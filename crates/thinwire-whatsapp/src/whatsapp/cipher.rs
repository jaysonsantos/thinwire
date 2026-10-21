// SPDX-License-Identifier: AGPL-3.0-only
//! Encrypt the WhatsApp device store inside this helper.
//!
//! The helper creates the key and reads it from the OS keychain. The key
//! does not cross the pipe. A test build of this library uses a fixed key.
//! The helper program does not.

use std::path::Path;
use std::sync::OnceLock;

use diesel::prelude::RunQueryDsl;
use whatsapp_rust::store::{SqliteDatabase, SqliteDatabaseConfig, SqliteStore};

#[cfg(not(test))]
use thinwire_core::{
    CreateStoreKeyError, LoadStoreKey, StoreKeyAction, WHATSAPP_STORE_KEY_ACCOUNT,
    create_store_key, load_store_key, store_key_action,
};
use thinwire_protocol::{
    MigrateOptions, PrepareError, is_encrypted_store, move_stores_aside, prepare_file,
    store_sibling,
};

/// Why the store was not opened. The app maps this to the account row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProtectFail {
    /// The keychain refused the read. The store was not moved.
    KeychainDenied,
    /// The encrypted store did not open. The store files were moved aside.
    /// `helper.lock` stays in the session folder.
    WrongKey,
    /// The keychain cannot keep a key. The store was not moved.
    NotSaved,
    /// The export or the row check failed. The plaintext store stays.
    EncryptFailed,
}

static SESSION_KEY: OnceLock<[u8; 32]> = OnceLock::new();

/// Copy `device.sqlite` to SQLCipher when it is plaintext.
///
/// # Errors
///
/// The string is the account-row sentence. A failure does not delete a store.
pub(super) fn protect_store(path: &Path) -> Result<(), ProtectFail> {
    let encrypted = is_encrypted_store(path)
        || store_sibling(path, ".enc-new").exists()
        || store_sibling(path, ".plain-old").exists();
    let key = resolve_key(encrypted, path)?;
    let _ = SESSION_KEY.set(key);
    match prepare_file(path, &key, MigrateOptions::default()) {
        Ok(_) => Ok(()),
        Err(PrepareError::WrongKey) => Err(move_store(path)),
        Err(PrepareError::Unreadable | PrepareError::CheckFailed | PrepareError::Io) => {
            Err(ProtectFail::EncryptFailed)
        }
    }
}

/// Open the device store with the raw key on every connection.
///
/// # Errors
///
/// Returns an error when the key is missing or the file does not open.
/// The error does not contain the key.
pub(super) async fn open_store(url: &str) -> Result<SqliteStore, ()> {
    let key = session_key()?;
    let literal = thinwire_protocol::raw_key_literal(&key);
    let config = SqliteDatabaseConfig::default().with_connection_init(move |conn| {
        let mut sql = format!("PRAGMA key = {literal};");
        let keyed = diesel::sql_query(&sql).execute(conn);
        wipe(&mut sql);
        keyed.map_err(|_| store_error())?;
        diesel::sql_query("SELECT count(*) FROM sqlite_master")
            .execute(conn)
            .map_err(|_| store_error())?;
        Ok(())
    });
    SqliteDatabase::open(url, config)
        .await
        .map(|database| database.store(1))
        .map_err(|_| ())
}

fn store_error() -> Box<dyn std::error::Error + Send + Sync> {
    "the session store did not open".into()
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
    match load_store_key(WHATSAPP_STORE_KEY_ACCOUNT) {
        LoadStoreKey::Found(key) => {
            let bytes = key.bytes();
            let _ = SESSION_KEY.set(bytes);
            Ok(bytes)
        }
        LoadStoreKey::Missing | LoadStoreKey::NotSaved | LoadStoreKey::Unavailable => Err(()),
    }
}

#[cfg(test)]
fn resolve_key(_encrypted: bool, _path: &Path) -> Result<[u8; 32], ProtectFail> {
    Ok(test_key())
}

#[cfg(test)]
fn test_key() -> [u8; 32] {
    [0xa5; 32]
}

#[cfg(not(test))]
fn resolve_key(encrypted: bool, path: &Path) -> Result<[u8; 32], ProtectFail> {
    let loaded = load_store_key(WHATSAPP_STORE_KEY_ACCOUNT);
    match store_key_action(&loaded, encrypted) {
        StoreKeyAction::Open => match loaded {
            LoadStoreKey::Found(key) => Ok(key.bytes()),
            LoadStoreKey::Missing | LoadStoreKey::NotSaved | LoadStoreKey::Unavailable => {
                Err(ProtectFail::KeychainDenied)
            }
        },
        StoreKeyAction::Denied => Err(ProtectFail::KeychainDenied),
        StoreKeyAction::NotSaved => Err(ProtectFail::NotSaved),
        StoreKeyAction::MoveAside => Err(move_store(path)),
        StoreKeyAction::Create => match create_store_key(WHATSAPP_STORE_KEY_ACCOUNT) {
            Ok(key) => Ok(key.bytes()),
            Err(CreateStoreKeyError::NotSaved) => Err(ProtectFail::NotSaved),
            Err(CreateStoreKeyError::Unavailable) => Err(ProtectFail::KeychainDenied),
        },
    }
}

fn move_store(store: &Path) -> ProtectFail {
    if let Some(dir) = store.parent()
        && dir.exists()
    {
        let _ = move_stores_aside(dir);
    }
    ProtectFail::WrongKey
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
    use thinwire_protocol::{is_plaintext_sqlite, raw_key_literal};

    #[test]
    fn a_denied_read_is_not_a_wrong_key() {
        assert_ne!(ProtectFail::KeychainDenied, ProtectFail::WrongKey);
        assert_ne!(ProtectFail::NotSaved, ProtectFail::WrongKey);
        assert_ne!(ProtectFail::KeychainDenied, ProtectFail::NotSaved);
        assert_ne!(ProtectFail::EncryptFailed, ProtectFail::KeychainDenied);
    }

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "thinwire-whatsapp-cipher-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    #[test]
    fn protect_encrypts_device_sqlite_and_keeps_the_rows() {
        let dir = temp_dir("device");
        let path = dir.join("device.sqlite");
        let conn = rusqlite::Connection::open(&path).expect("plain");
        conn.execute_batch("CREATE TABLE items (id INTEGER PRIMARY KEY);")
            .expect("schema");
        conn.execute("INSERT INTO items (id) VALUES (1)", [])
            .expect("row");
        conn.execute("INSERT INTO items (id) VALUES (2)", [])
            .expect("row");
        drop(conn);
        protect_store(&path).expect("protect");
        assert!(!is_plaintext_sqlite(&path));
        let opened = rusqlite::Connection::open(&path).expect("enc");
        let mut sql = format!("PRAGMA key = {};", raw_key_literal(&test_key()));
        opened.execute_batch(&sql).expect("key");
        sql.clear();
        let count: i64 = opened
            .query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
            .expect("count");
        assert_eq!(count, 2);
        assert!(!store_sibling(&path, "-wal").exists());
        assert!(!store_sibling(&path, "-shm").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_wrong_key_moves_the_whatsapp_folder_aside() {
        let parent = temp_dir("aside");
        let dir = parent.join("whatsapp");
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("device.sqlite");
        let conn = rusqlite::Connection::open(&path).expect("plain");
        conn.execute_batch("CREATE TABLE items (id INTEGER PRIMARY KEY);")
            .expect("schema");
        drop(conn);
        let other = [0x11u8; 32];
        prepare_file(&path, &other, MigrateOptions::default()).expect("encrypt");
        let error = protect_store(&path).expect_err("wrong key");
        assert_eq!(error, ProtectFail::WrongKey);
        assert!(dir.is_dir(), "the session directory stays");
        assert!(!path.exists());
        let moved = std::fs::read_dir(&parent)
            .expect("parent")
            .flatten()
            .any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("whatsapp.stale-")
            });
        assert!(moved);
        let _ = std::fs::remove_dir_all(&parent);
    }

    #[test]
    fn a_second_helper_cannot_lock_the_session_after_a_wrong_key() {
        let parent = temp_dir("lock");
        let dir = parent.join("whatsapp");
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("device.sqlite");
        let conn = rusqlite::Connection::open(&path).expect("plain");
        conn.execute_batch("CREATE TABLE items (id INTEGER PRIMARY KEY);")
            .expect("schema");
        drop(conn);
        let other = [0x11u8; 32];
        prepare_file(&path, &other, MigrateOptions::default()).expect("encrypt");
        let held = super::super::path::lock_session_at(&dir).expect("first helper");
        let error = protect_store(&path).expect_err("wrong key");
        assert_eq!(error, ProtectFail::WrongKey);
        assert!(dir.join("helper.lock").is_file());
        assert!(!path.exists());
        let second = super::super::path::lock_session_at(&dir);
        assert_eq!(
            second.expect_err("second helper"),
            super::super::path::SessionLockError::InUse
        );
        drop(held);
        let _ = std::fs::remove_dir_all(&parent);
    }

    #[tokio::test]
    async fn a_new_store_opened_with_the_key_is_encrypted() {
        let dir = temp_dir("new");
        let path = dir.join("device.sqlite");
        let url = path.to_str().expect("url").to_string();
        let _ = SESSION_KEY.set(test_key());
        let store = open_store(&url).await.expect("open");
        drop(store);
        assert!(path.is_file());
        assert!(!is_plaintext_sqlite(&path));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
