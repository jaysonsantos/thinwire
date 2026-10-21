// SPDX-License-Identifier: AGPL-3.0-only
//! Encrypt the Signal session stores inside this helper.
//!
//! One keychain key opens the live store, the pending link file, and every
//! retained store. The key does not cross the pipe. A test build of this
//! library uses a fixed key. The helper program does not: that program reads
//! the OS keychain and refuses a keychain that cannot save.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use super::path::{PENDING_FILE, RETAINED_DIR, SQLITE_FILE};

#[cfg(not(test))]
use thinwire_core::{
    CreateStoreKeyError, LoadStoreKey, SIGNAL_STORE_KEY_ACCOUNT, create_store_key, load_store_key,
};
use thinwire_protocol::{
    MigrateOptions, PrepareError, is_encrypted_store, prepare_file, signal_encrypt_failed,
    signal_key_wrong, signal_key_wrong_kept,
};
#[cfg(not(test))]
use thinwire_protocol::{signal_key_not_saved, signal_keychain_unavailable};

static SESSION_KEY: OnceLock<[u8; 32]> = OnceLock::new();

/// Copy every existing store to SQLCipher, then keep the key for later opens.
///
/// # Errors
///
/// The string is the account-row sentence. A failure does not delete a store.
pub(super) fn protect_session(dir: &Path) -> Result<(), String> {
    let files = store_files(dir);
    let key = resolve_key(files.iter().any(|path| is_encrypted_store(path)), dir)?;
    let _ = SESSION_KEY.set(key);
    for path in &files {
        if is_encrypted_store(path) {
            open_or_move(dir, path, &key)?;
        }
    }
    for path in &files {
        if is_encrypted_store(path) {
            continue;
        }
        match prepare_file(path, &key, MigrateOptions::default()) {
            Ok(_) => {}
            Err(PrepareError::WrongKey) => return Err(move_aside_detail(dir)),
            Err(PrepareError::CheckFailed | PrepareError::Io) => {
                return Err(signal_encrypt_failed().to_string());
            }
        }
    }
    Ok(())
}

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

fn open_or_move(dir: &Path, path: &Path, key: &[u8; 32]) -> Result<(), String> {
    match prepare_file(path, key, MigrateOptions::default()) {
        Ok(_) => Ok(()),
        Err(PrepareError::WrongKey) => Err(move_aside_detail(dir)),
        Err(PrepareError::CheckFailed | PrepareError::Io) => {
            Err(signal_encrypt_failed().to_string())
        }
    }
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
fn resolve_key(_encrypted_present: bool, _dir: &Path) -> Result<[u8; 32], String> {
    Ok(test_key())
}

#[cfg(test)]
fn test_key() -> [u8; 32] {
    [0x5a; 32]
}

#[cfg(not(test))]
fn resolve_key(encrypted_present: bool, dir: &Path) -> Result<[u8; 32], String> {
    match load_store_key(SIGNAL_STORE_KEY_ACCOUNT) {
        LoadStoreKey::Found(key) => Ok(key.bytes()),
        LoadStoreKey::Unavailable => Err(signal_keychain_unavailable().to_string()),
        LoadStoreKey::NotSaved => Err(signal_key_not_saved().to_string()),
        LoadStoreKey::Missing if encrypted_present => Err(move_aside_detail(dir)),
        LoadStoreKey::Missing => match create_store_key(SIGNAL_STORE_KEY_ACCOUNT) {
            Ok(key) => Ok(key.bytes()),
            Err(CreateStoreKeyError::NotSaved) => Err(signal_key_not_saved().to_string()),
            Err(CreateStoreKeyError::Unavailable) => Err(signal_keychain_unavailable().to_string()),
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

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    name.push(suffix);
    path.with_file_name(name)
}

fn move_aside_detail(dir: &Path) -> String {
    match rename_aside(dir) {
        Ok(folder) => signal_key_wrong(&folder),
        Err(()) => signal_key_wrong_kept().to_string(),
    }
}

fn rename_aside(dir: &Path) -> Result<String, ()> {
    let parent = dir.parent().ok_or(())?;
    let name = dir.file_name().ok_or(())?;
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let base = format!("{}.stale-{secs}", name.to_string_lossy());
    for suffix in [String::new()]
        .into_iter()
        .chain((2..1_000).map(|index| format!("-{index}")))
    {
        let folder = format!("{base}{suffix}");
        let dest = parent.join(&folder);
        if dest.exists() {
            continue;
        }
        return std::fs::rename(dir, &dest).map(|()| folder).map_err(|_| ());
    }
    Err(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use thinwire_protocol::is_plaintext_sqlite;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "thinwire-signal-cipher-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
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
        assert!(error.contains("moved aside"));
        assert!(!dir.exists());
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
}
