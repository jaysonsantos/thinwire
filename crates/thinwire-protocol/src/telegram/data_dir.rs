//! The TDLib database folder. Recovery when its key is lost.
//!
//! TDLib encrypts its database with a key from the vault. When the vault
//! lost that key (a memory-only keychain, or kernel keyutils after a restart),
//! the old folder can never open again. It is moved aside, never deleted, so
//! a new login can start in a fresh folder.

#![cfg_attr(not(feature = "telegram-tdlib"), allow(dead_code))]

use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Name part for a moved-aside folder: `tdlib.stale-<unix seconds>`.
const STALE_MARK: &str = "stale";

/// Most tries to find a free stale name in one second.
const STALE_NAME_TRIES: u32 = 100;

/// Throwaway folder for one process when the keychain cannot save the key.
/// Its key lives in memory only, so the data cannot open after exit.
#[must_use]
pub(super) fn session_dir(temp: &Path, pid: u32) -> PathBuf {
    temp.join(format!("thinwire-tdlib-session-{pid}"))
}

/// [`session_dir`] for this process, in the OS temp folder.
#[must_use]
pub(super) fn this_process_session_dir() -> PathBuf {
    session_dir(&std::env::temp_dir(), std::process::id())
}

/// True when the folder exists and holds any entry.
#[must_use]
pub(super) fn has_data(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_some())
}

/// Move the folder aside if its key is gone. Returns where it went.
pub(super) fn move_aside_if_keyless(dir: &Path, has_key: bool) -> io::Result<Option<PathBuf>> {
    if has_key || !has_data(dir) {
        return Ok(None);
    }
    move_aside(dir).map(Some)
}

/// Rename `dir` to a free sibling `<name>.stale-<unix seconds>[-n]`.
pub(super) fn move_aside(dir: &Path) -> io::Result<PathBuf> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let target = free_stale_path(dir, now)?;
    std::fs::rename(dir, &target)?;
    Ok(target)
}

fn free_stale_path(dir: &Path, unix_secs: u64) -> io::Result<PathBuf> {
    let name = dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("tdlib");
    for attempt in 0..STALE_NAME_TRIES {
        let candidate = if attempt == 0 {
            format!("{name}.{STALE_MARK}-{unix_secs}")
        } else {
            format!("{name}.{STALE_MARK}-{unix_secs}-{attempt}")
        };
        let path = dir.with_file_name(candidate);
        if !path.exists() {
            return Ok(path);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "no free name for the old Telegram data folder",
    ))
}

/// TDLib text that points at the database or its key. Such an error can be
/// fixed by a fresh folder; other errors (for example a bad api_id) cannot.
#[must_use]
pub(super) fn is_database_error(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("database") || message.contains("encryption")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        let root = std::env::temp_dir().join(format!(
            "thinwire-data-dir-{name}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("scratch root");
        root
    }

    #[test]
    fn a_keyless_folder_with_data_moves_aside_and_is_kept() {
        let root = scratch("keyless");
        let dir = root.join("tdlib");
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(dir.join("td.binlog"), b"old").expect("file");

        assert_eq!(move_aside_if_keyless(&dir, true).expect("keep"), None);
        assert!(dir.exists(), "a key exists, so the folder stays");

        let moved = move_aside_if_keyless(&dir, false)
            .expect("move")
            .expect("moved");
        assert!(!dir.exists());
        assert!(moved.join("td.binlog").exists(), "moved, not deleted");
        let name = moved.file_name().and_then(|n| n.to_str()).expect("name");
        assert!(name.starts_with("tdlib.stale-"), "{name}");
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn an_empty_or_missing_folder_is_left_alone() {
        let root = scratch("empty");
        let dir = root.join("tdlib");
        assert_eq!(move_aside_if_keyless(&dir, false).expect("missing"), None);
        std::fs::create_dir_all(&dir).expect("dir");
        assert_eq!(move_aside_if_keyless(&dir, false).expect("empty"), None);
        assert!(dir.exists());
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn a_second_move_in_the_same_second_gets_a_new_name() {
        let root = scratch("twice");
        let dir = root.join("tdlib");
        let first = free_stale_path(&dir, 7).expect("first");
        std::fs::create_dir_all(&first).expect("taken");
        let second = free_stale_path(&dir, 7).expect("second");
        assert_ne!(first, second);
        assert!(second.to_string_lossy().ends_with("tdlib.stale-7-1"));
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn a_memory_only_session_gets_its_own_temp_folder() {
        let temp = Path::new("/tmp");
        let first = session_dir(temp, 41);
        assert_eq!(first, Path::new("/tmp/thinwire-tdlib-session-41"));
        assert_ne!(first, session_dir(temp, 42), "one folder per process");
    }

    #[test]
    fn only_database_errors_trigger_a_fresh_folder() {
        assert!(is_database_error("Wrong database encryption key"));
        assert!(is_database_error("Can't open database"));
        assert!(!is_database_error("Valid api_id must be provided"));
        assert!(!is_database_error("PHONE_NUMBER_INVALID"));
    }
}
