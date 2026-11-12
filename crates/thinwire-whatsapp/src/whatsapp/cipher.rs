// SPDX-License-Identifier: AGPL-3.0-only
//! Encrypt the WhatsApp device store inside this helper.
//!
//! The helper creates the key and reads it from the OS keychain. The key
//! does not cross the pipe. A test build of this library uses a fixed key.
//! The helper program does not.

use std::path::Path;

use diesel::prelude::RunQueryDsl;
use whatsapp_rust::store::{SqliteDatabase, SqliteDatabaseConfig, SqliteStore};

use thinwire_core::SessionKey;
#[cfg(not(test))]
use thinwire_core::WHATSAPP_STORE_KEY_ACCOUNT;
use thinwire_protocol::{
    MigrateOptions, PrepareError, is_encrypted_store, move_stores_aside, prepare_file,
    store_sibling,
};

pub(super) use thinwire_core::StoreProtectFail as ProtectFail;

static SESSION_KEY: SessionKey = SessionKey::new();

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
    SESSION_KEY.keep(key);
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

/// The session key, for the chat cache beside the device store. The open
/// of the device store reads it first.
pub(super) fn cache_key() -> Result<[u8; 32], ()> {
    session_key()
}

/// The key of this helper process: the keychain entry, read one time. A
/// test build has a fixed key and reads no keychain.
fn session_key() -> Result<[u8; 32], ()> {
    #[cfg(test)]
    let key = Some(SESSION_KEY.keep(test_key()));
    #[cfg(not(test))]
    let key = SESSION_KEY.saved(&super::path::account_key_name(WHATSAPP_STORE_KEY_ACCOUNT));
    key.ok_or(())
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
    let account = super::path::account_key_name(WHATSAPP_STORE_KEY_ACCOUNT);
    thinwire_core::resolve_store_key(&account, encrypted, || store_moved(path))
        .map(|key| key.bytes())
}

fn move_store(store: &Path) -> ProtectFail {
    ProtectFail::after_move(store_moved(store))
}

/// Move the files of the folder of `store` aside. `false` when that folder
/// is not there, or when a file did not move.
fn store_moved(store: &Path) -> bool {
    store
        .parent()
        .filter(|dir| dir.exists())
        .is_some_and(|dir| move_stores_aside(dir).is_ok())
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
        assert_ne!(ProtectFail::NotMoved, ProtectFail::WrongKey);
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
    fn a_failed_move_aside_is_not_a_wrong_key() {
        let parent = temp_dir("stuck");
        let dir = parent.join("whatsapp");
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("device.sqlite");
        let conn = rusqlite::Connection::open(&path).expect("plain");
        conn.execute_batch("CREATE TABLE items (id INTEGER PRIMARY KEY);")
            .expect("schema");
        drop(conn);
        let other = [0x11u8; 32];
        prepare_file(&path, &other, MigrateOptions::default()).expect("encrypt");
        let before = std::fs::read(&path).expect("bytes");
        // Filling every stale-<second> name loses to the clock on a slow disk.
        // A parent that cannot take a new entry fails the first create_dir.
        let block = ParentBlock::lock(&parent);
        let error = protect_store(&path).expect_err("move failed");
        assert_eq!(error, ProtectFail::NotMoved);
        assert_ne!(error, ProtectFail::WrongKey);
        assert_eq!(std::fs::read(&path).expect("left in place"), before);
        drop(block);
        let _ = std::fs::remove_dir_all(&parent);
    }

    /// The parent cannot gain a stale folder until this value is dropped.
    struct ParentBlock<'a> {
        parent: &'a std::path::Path,
        #[cfg(unix)]
        mode: u32,
        #[cfg(windows)]
        user: String,
    }

    impl<'a> ParentBlock<'a> {
        fn lock(parent: &'a std::path::Path) -> Self {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(parent)
                    .expect("meta")
                    .permissions()
                    .mode();
                let mut perms = std::fs::metadata(parent).expect("meta").permissions();
                perms.set_mode(mode & !0o222);
                std::fs::set_permissions(parent, perms).expect("chmod");
                let blocked = Self { parent, mode };
                assert!(
                    std::fs::create_dir(parent.join("cannot-create-stale")).is_err(),
                    "the parent still accepts a new folder"
                );
                blocked
            }
            #[cfg(windows)]
            {
                let user = current_windows_user();
                let status = std::process::Command::new("icacls")
                    .arg(parent)
                    .arg("/deny")
                    .arg(format!("{user}:(AD)"))
                    .status()
                    .expect("icacls");
                assert!(status.success(), "icacls deny");
                let blocked = Self { parent, user };
                assert!(
                    std::fs::create_dir(parent.join("cannot-create-stale")).is_err(),
                    "the parent still accepts a new folder"
                );
                blocked
            }
        }
    }

    impl Drop for ParentBlock<'_> {
        fn drop(&mut self) {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = std::fs::metadata(self.parent).expect("meta").permissions();
                perms.set_mode(self.mode);
                std::fs::set_permissions(self.parent, perms).expect("chmod");
            }
            #[cfg(windows)]
            {
                let status = std::process::Command::new("icacls")
                    .arg(self.parent)
                    .arg("/remove:d")
                    .arg(&self.user)
                    .status()
                    .expect("icacls");
                assert!(status.success(), "icacls remove deny");
            }
        }
    }

    #[cfg(windows)]
    fn current_windows_user() -> String {
        let output = std::process::Command::new("whoami")
            .output()
            .expect("whoami");
        assert!(output.status.success(), "whoami");
        String::from_utf8(output.stdout)
            .expect("utf8")
            .trim()
            .to_string()
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
        SESSION_KEY.keep(test_key());
        let store = open_store(&url).await.expect("open");
        drop(store);
        assert!(path.is_file());
        assert!(!is_plaintext_sqlite(&path));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
