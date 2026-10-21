//! Copy a plaintext SQLite store into a new SQLCipher file.
//!
//! The helper does not encrypt a store in place. The order is fixed:
//! checkpoint the plaintext file, export into a new file, check every table,
//! rename the new file into place, then delete the plaintext file.
//! A crash at any step leaves a store that the next start can open.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::Connection;

/// First 16 bytes of a plaintext SQLite file.
const SQLITE_HEADER: &[u8; 16] = b"SQLite format 3\0";

const ENC_NEW_SUFFIX: &str = ".enc-new";
const PLAIN_OLD_SUFFIX: &str = ".plain-old";

/// Where a test stops the copy. A release call uses [`MigrateStop::None`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrateStop {
    None,
    /// The new file exists. The plaintext file is still in place.
    AfterExport,
    /// The row check passed. The plaintext file is still in place.
    AfterCheck,
    /// The plaintext file was renamed aside. The new file is not in place yet.
    AfterMovePlaintext,
    /// The new file is in place. The plaintext file is still aside.
    AfterRename,
}

/// Controls for one copy. Release code uses [`MigrateOptions::default`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MigrateOptions {
    pub stop: MigrateStop,
    /// The row check fails on purpose. The plaintext file stays.
    pub force_mismatch: bool,
}

impl Default for MigrateOptions {
    fn default() -> Self {
        Self {
            stop: MigrateStop::None,
            force_mismatch: false,
        }
    }
}

/// The copy did not finish. The error does not contain the key or a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrepareError {
    /// The row check failed. The plaintext file is unchanged.
    CheckFailed,
    /// The file is encrypted and this key does not open it. The file stays.
    WrongKey,
    /// A file step failed. An encrypted store was not deleted.
    Io,
}

/// The file is ready, or a test stopped between steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrepareOutcome {
    Ready,
    Stopped,
}

/// `true` when the file starts with the plaintext SQLite header.
#[must_use]
pub fn is_plaintext_sqlite(path: &Path) -> bool {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(_) => return false,
    };
    let mut header = [0u8; 16];
    file.read_exact(&mut header).is_ok() && header == *SQLITE_HEADER
}

/// `true` when the file has bytes and is not a plaintext SQLite file.
#[must_use]
pub fn is_encrypted_store(path: &Path) -> bool {
    match fs::metadata(path) {
        Ok(meta) if meta.is_file() && meta.len() > 0 => !is_plaintext_sqlite(path),
        _ => false,
    }
}

/// SQL text for a raw SQLCipher key: `"x'<64 hex chars>'"`.
///
/// The caller passes this text to `PRAGMA key` or `ATTACH ... KEY`.
/// The caller does not write this text to a log or an error.
#[must_use]
pub fn raw_key_literal(key: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(69);
    out.push('"');
    out.push('x');
    out.push('\'');
    for byte in key {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out.push('\'');
    out.push('"');
    out
}

/// Make `path` an encrypted store that opens with `key`.
///
/// A missing file is ready: the caller creates it with the key.
/// An encrypted file that does not open returns [`PrepareError::WrongKey`].
///
/// # Errors
///
/// See [`PrepareError`]. A failed export deletes only the partial new file.
pub fn prepare_file(
    path: &Path,
    key: &[u8; 32],
    options: MigrateOptions,
) -> Result<PrepareOutcome, PrepareError> {
    let enc_new = sibling(path, ENC_NEW_SUFFIX);
    let plain_old = sibling(path, PLAIN_OLD_SUFFIX);

    if !path.exists() {
        return resume_without_canonical(path, &plain_old, &enc_new, key, options);
    }
    if is_plaintext_sqlite(path) {
        return migrate_plaintext(path, &enc_new, &plain_old, key, options);
    }
    match fs::metadata(path) {
        Ok(meta) if meta.is_file() && meta.len() == 0 => {
            remove_set(path)?;
            let _ = remove_set(&enc_new);
            let _ = remove_set(&plain_old);
            return Ok(PrepareOutcome::Ready);
        }
        Ok(meta) if meta.is_file() => {}
        _ => return Err(PrepareError::Io),
    }
    if opens_with_key(path, key) {
        if enc_new.exists() {
            remove_set(&enc_new)?;
        }
        if plain_old.exists() {
            remove_set(&plain_old)?;
        }
        return Ok(PrepareOutcome::Ready);
    }
    Err(PrepareError::WrongKey)
}

fn resume_without_canonical(
    path: &Path,
    plain_old: &Path,
    enc_new: &Path,
    key: &[u8; 32],
    options: MigrateOptions,
) -> Result<PrepareOutcome, PrepareError> {
    if plain_old.exists() && enc_new.exists() {
        return finish_checked_swap(path, plain_old, enc_new, key, options);
    }
    if plain_old.exists() {
        rename_set(plain_old, path)?;
        return migrate_plaintext(path, enc_new, plain_old, key, options);
    }
    if enc_new.exists() {
        if opens_with_key(enc_new, key) {
            rename_set(enc_new, path)?;
            return Ok(PrepareOutcome::Ready);
        }
        remove_set(enc_new)?;
        return Err(PrepareError::CheckFailed);
    }
    Ok(PrepareOutcome::Ready)
}

fn finish_checked_swap(
    path: &Path,
    plain_old: &Path,
    enc_new: &Path,
    key: &[u8; 32],
    options: MigrateOptions,
) -> Result<PrepareOutcome, PrepareError> {
    let matched = !options.force_mismatch && counts_match(plain_old, enc_new, key).unwrap_or(false);
    if !matched {
        restore_plaintext(path, plain_old, enc_new)?;
        return Err(PrepareError::CheckFailed);
    }
    rename_set(enc_new, path)?;
    if options.stop == MigrateStop::AfterRename {
        return Ok(PrepareOutcome::Stopped);
    }
    remove_set(plain_old)?;
    Ok(PrepareOutcome::Ready)
}

fn restore_plaintext(path: &Path, plain_old: &Path, enc_new: &Path) -> Result<(), PrepareError> {
    if enc_new.exists() {
        remove_set(enc_new)?;
    }
    if !path.exists() && plain_old.exists() {
        rename_set(plain_old, path)?;
    }
    Ok(())
}

fn migrate_plaintext(
    path: &Path,
    enc_new: &Path,
    plain_old: &Path,
    key: &[u8; 32],
    options: MigrateOptions,
) -> Result<PrepareOutcome, PrepareError> {
    if enc_new.exists() {
        remove_set(enc_new)?;
    }
    export_encrypted(path, enc_new, key)?;
    if options.stop == MigrateStop::AfterExport {
        return Ok(PrepareOutcome::Stopped);
    }
    let matched = !options.force_mismatch && counts_match(path, enc_new, key)?;
    if !matched {
        remove_set(enc_new)?;
        return Err(PrepareError::CheckFailed);
    }
    if options.stop == MigrateStop::AfterCheck {
        return Ok(PrepareOutcome::Stopped);
    }
    if plain_old.exists() {
        remove_set(plain_old)?;
    }
    rename_set(path, plain_old)?;
    if options.stop == MigrateStop::AfterMovePlaintext {
        return Ok(PrepareOutcome::Stopped);
    }
    // A failed rename leaves the plaintext file aside. The next start finishes
    // the swap or restores that file.
    rename_set(enc_new, path)?;
    if options.stop == MigrateStop::AfterRename {
        return Ok(PrepareOutcome::Stopped);
    }
    remove_set(plain_old)?;
    Ok(PrepareOutcome::Ready)
}

fn export_encrypted(plain: &Path, dest: &Path, key: &[u8; 32]) -> Result<(), PrepareError> {
    let conn = Connection::open(plain).map_err(|_| PrepareError::Io)?;
    let _ = conn.busy_timeout(Duration::from_secs(2));
    if conn
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
        .is_err()
    {
        return Err(PrepareError::Io);
    }
    let quoted = dest
        .to_string_lossy()
        .replace('\\', "/")
        .replace('\'', "''");
    let mut attach = format!(
        "ATTACH DATABASE '{quoted}' AS encrypted KEY {};",
        raw_key_literal(key)
    );
    let attached = conn.execute_batch(&attach);
    wipe(&mut attach);
    if attached.is_err() {
        drop(conn);
        let _ = remove_set(dest);
        return Err(PrepareError::Io);
    }
    let exported = conn.query_row("SELECT sqlcipher_export('encrypted')", [], |_| Ok(()));
    let detached = conn.execute_batch("DETACH DATABASE encrypted");
    drop(conn);
    if exported.is_err() || detached.is_err() {
        let _ = remove_set(dest);
        return Err(PrepareError::Io);
    }
    Ok(())
}

fn counts_match(plain: &Path, encrypted: &Path, key: &[u8; 32]) -> Result<bool, PrepareError> {
    let plain_conn = Connection::open(plain).map_err(|_| PrepareError::Io)?;
    let plain_counts = table_counts(&plain_conn)?;
    drop(plain_conn);
    let Ok(enc_conn) = open_keyed(encrypted, key) else {
        return Ok(false);
    };
    let enc_counts = table_counts(&enc_conn)?;
    Ok(plain_counts == enc_counts)
}

fn table_counts(conn: &Connection) -> Result<BTreeMap<String, i64>, PrepareError> {
    let mut stmt = conn
        .prepare(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .map_err(|_| PrepareError::Io)?;
    let names = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|_| PrepareError::Io)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| PrepareError::Io)?;
    drop(stmt);
    let mut counts = BTreeMap::new();
    for name in names {
        let sql = format!("SELECT COUNT(*) FROM {}", quote_ident(&name));
        let count: i64 = conn
            .query_row(&sql, [], |row| row.get(0))
            .map_err(|_| PrepareError::Io)?;
        counts.insert(name, count);
    }
    Ok(counts)
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn open_keyed(path: &Path, key: &[u8; 32]) -> Result<Connection, ()> {
    let conn = Connection::open(path).map_err(|_| ())?;
    let _ = conn.busy_timeout(Duration::from_secs(2));
    apply_key(&conn, key)?;
    conn.query_row("SELECT count(*) FROM sqlite_master", [], |_| Ok(()))
        .map_err(|_| ())?;
    Ok(conn)
}

fn opens_with_key(path: &Path, key: &[u8; 32]) -> bool {
    open_keyed(path, key).is_ok()
}

fn apply_key(conn: &Connection, key: &[u8; 32]) -> Result<(), ()> {
    let mut sql = format!("PRAGMA key = {};", raw_key_literal(key));
    let result = conn.execute_batch(&sql);
    wipe(&mut sql);
    result.map(|_| ()).map_err(|_| ())
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    name.push(suffix);
    path.with_file_name(name)
}

fn rename_set(from: &Path, to: &Path) -> Result<(), PrepareError> {
    if to.exists() {
        return Err(PrepareError::Io);
    }
    fs::rename(from, to).map_err(|_| PrepareError::Io)?;
    for extra in ["-wal", "-shm", "-journal"] {
        let src = sibling(from, extra);
        if !src.exists() {
            continue;
        }
        let dst = sibling(to, extra);
        if fs::rename(&src, &dst).is_err() {
            let _ = fs::rename(to, from);
            return Err(PrepareError::Io);
        }
    }
    Ok(())
}

fn remove_set(path: &Path) -> Result<(), PrepareError> {
    for extra in ["", "-wal", "-shm", "-journal"] {
        let file = if extra.is_empty() {
            path.to_path_buf()
        } else {
            sibling(path, extra)
        };
        if file.exists() {
            fs::remove_file(&file).map_err(|_| PrepareError::Io)?;
        }
    }
    Ok(())
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

    const KEY: [u8; 32] = [0x11; 32];
    const OTHER: [u8; 32] = [0x22; 32];

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "thinwire-cipher-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        fs::create_dir_all(&dir).expect("dir");
        dir
    }

    fn seed(path: &Path, notes: i64) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent");
        }
        let conn = Connection::open(path).expect("plain");
        conn.execute_batch(
            "\
PRAGMA journal_mode=WAL;
CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT NOT NULL);
CREATE TABLE tags (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
",
        )
        .expect("schema");
        for id in 1..=notes {
            conn.execute("INSERT INTO notes (id, body) VALUES (?1, ?2)", (id, "n"))
                .expect("note");
        }
        conn.execute("INSERT INTO tags (id, name) VALUES (1, 'a')", [])
            .expect("tag");
        drop(conn);
    }

    fn note_count(path: &Path, key: &[u8; 32]) -> i64 {
        let conn = open_keyed(path, key).expect("open");
        conn.query_row("SELECT COUNT(*) FROM notes", [], |row| row.get(0))
            .expect("count")
    }

    fn leftovers(path: &Path) -> Vec<String> {
        let parent = path.parent().expect("parent");
        let stem = path
            .file_name()
            .expect("name")
            .to_string_lossy()
            .to_string();
        let mut names = Vec::new();
        for entry in fs::read_dir(parent).expect("read") {
            let name = entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned();
            if name.starts_with(&stem) && name != stem {
                names.push(name);
            }
        }
        names.sort();
        names
    }

    #[test]
    fn the_raw_key_literal_is_the_sqlcipher_form() {
        let literal = raw_key_literal(&KEY);
        assert_eq!(literal.len(), 69);
        assert!(literal.starts_with("\"x'"));
        assert!(literal.ends_with("'\""));
        assert_eq!(literal.matches("11").count(), 32);
    }

    #[test]
    fn a_plaintext_store_becomes_encrypted_and_keeps_its_rows() {
        let dir = temp_dir("ok");
        let path = dir.join("session.sqlite");
        seed(&path, 3);
        let outcome = prepare_file(&path, &KEY, MigrateOptions::default()).expect("copy");
        assert_eq!(outcome, PrepareOutcome::Ready);
        assert!(is_encrypted_store(&path));
        assert!(!is_plaintext_sqlite(&path));
        assert_eq!(note_count(&path, &KEY), 3);
        assert!(leftovers(&path).is_empty(), "{:?}", leftovers(&path));
        let again = prepare_file(&path, &KEY, MigrateOptions::default()).expect("second");
        assert_eq!(again, PrepareOutcome::Ready);
        assert_eq!(note_count(&path, &KEY), 3);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_check_keeps_the_plaintext_store() {
        let dir = temp_dir("mismatch");
        let path = dir.join("session.sqlite");
        seed(&path, 4);
        let error = prepare_file(
            &path,
            &KEY,
            MigrateOptions {
                stop: MigrateStop::None,
                force_mismatch: true,
            },
        )
        .expect_err("check");
        assert_eq!(error, PrepareError::CheckFailed);
        assert!(is_plaintext_sqlite(&path));
        assert!(!sibling(&path, ENC_NEW_SUFFIX).exists());
        assert!(!sibling(&path, PLAIN_OLD_SUFFIX).exists());
        let conn = Connection::open(&path).expect("plain");
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM notes", [], |row| row.get(0))
            .expect("count");
        assert_eq!(count, 4);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_crash_between_steps_still_opens_on_the_next_start() {
        for stop in [
            MigrateStop::AfterExport,
            MigrateStop::AfterCheck,
            MigrateStop::AfterMovePlaintext,
            MigrateStop::AfterRename,
        ] {
            let dir = temp_dir("crash");
            let path = dir.join("session.sqlite");
            seed(&path, 2);
            let stopped = prepare_file(
                &path,
                &KEY,
                MigrateOptions {
                    stop,
                    force_mismatch: false,
                },
            )
            .expect("stop");
            assert_eq!(stopped, PrepareOutcome::Stopped);
            let ready = prepare_file(&path, &KEY, MigrateOptions::default()).expect("resume");
            assert_eq!(ready, PrepareOutcome::Ready);
            assert!(is_encrypted_store(&path));
            assert!(!is_plaintext_sqlite(&path));
            assert_eq!(note_count(&path, &KEY), 2);
            assert!(
                leftovers(&path).is_empty(),
                "{stop:?} left {:?}",
                leftovers(&path)
            );
            let _ = fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn a_wrong_key_does_not_delete_an_encrypted_store() {
        let dir = temp_dir("wrong");
        let path = dir.join("device.sqlite");
        seed(&path, 1);
        prepare_file(&path, &KEY, MigrateOptions::default()).expect("copy");
        let error = prepare_file(&path, &OTHER, MigrateOptions::default()).expect_err("key");
        assert_eq!(error, PrepareError::WrongKey);
        assert!(path.is_file());
        assert!(is_encrypted_store(&path));
        assert_eq!(note_count(&path, &KEY), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_file_is_ready_and_stays_missing() {
        let dir = temp_dir("missing");
        let path = dir.join("session.sqlite");
        let outcome = prepare_file(&path, &KEY, MigrateOptions::default()).expect("missing");
        assert_eq!(outcome, PrepareOutcome::Ready);
        assert!(!path.exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
