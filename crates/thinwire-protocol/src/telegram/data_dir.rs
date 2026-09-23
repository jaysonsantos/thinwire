//! The TDLib database folder. Recovery when its key is lost.
//!
//! TDLib encrypts its database with a key from the vault. When the vault
//! lost that key (a memory-only keychain, or kernel keyutils after a restart),
//! the old folder can never open again. It is moved aside, never deleted, so
//! a new login can start in a fresh folder.

#![cfg_attr(not(feature = "telegram-tdlib"), allow(dead_code))]

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// Name part for a moved-aside folder: `tdlib.stale-<unix seconds>`.
const STALE_MARK: &str = "stale";

/// Most tries to find a free stale name in one second.
const STALE_NAME_TRIES: u32 = 100;

/// Name start of a throwaway session folder.
const SESSION_PREFIX: &str = "thinwire-tdlib-session-";

/// Random bytes in a session folder name, so another user cannot guess it.
const SESSION_SUFFIX_BYTES: usize = 8;

/// The throwaway folder of this process, once made.
static SESSION_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Throwaway folder for this process when the keychain cannot save the key.
/// Its key lives in memory only, so the data cannot open after exit.
///
/// It is a new folder with a random name under `$XDG_RUNTIME_DIR` (per user)
/// or the temp folder. It must not exist before, must not be a symlink, and
/// must be private to this user; else it is refused.
pub(super) fn this_process_session_dir() -> io::Result<PathBuf> {
    if let Some(dir) = SESSION_DIR.get() {
        verify_private(dir)?;
        return Ok(dir.clone());
    }
    let dir = session_base().join(format!("{SESSION_PREFIX}{}", random_suffix()?));
    create_private_dir(&dir)?;
    Ok(SESSION_DIR.get_or_init(|| dir).clone())
}

/// Remove this process's throwaway folder at a clean exit. It can never
/// open again, and it holds TDLib's unencrypted media cache.
pub(super) fn remove_this_process_session_dir() {
    let Some(dir) = SESSION_DIR.get() else {
        return;
    };
    if verify_private(dir).is_ok()
        && let Err(error) = fs::remove_dir_all(dir)
    {
        tracing::warn!(%error, "throwaway telegram folder was not removed");
    }
}

fn session_base() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute() && verify_private(dir).is_ok())
        .unwrap_or_else(std::env::temp_dir)
}

fn random_suffix() -> io::Result<String> {
    let mut bytes = [0u8; SESSION_SUFFIX_BYTES];
    getrandom::fill(&mut bytes).map_err(|error| io::Error::other(error.to_string()))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Create a new folder, mode 0700. Fails when anything (a folder, a file,
/// or a symlink) is already at the path.
pub(super) fn create_private_dir(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)?;
    verify_private(path)
}

/// A real folder (not a symlink) with no access for other users.
///
/// No owner check is needed: `create_private_dir` fails when anything is
/// already at the path, so a folder it made is always this user's. A base
/// folder that another user owns with mode 0700 cannot be written into.
fn verify_private(path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(io::Error::other("not a private folder"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.mode() & 0o077 != 0 {
            return Err(io::Error::other("folder is open to other users"));
        }
    }
    Ok(())
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

/// TDLib's text when the key does not open the database.
const WRONG_KEY_ERROR: &str = "wrong database encryption key";

/// Text of a lock error: another client (for example a second thinwire)
/// uses the folder. Such a folder is alive and must never move.
const LOCK_MARKERS: [&str; 2] = ["already in use", "lock"];

/// Only TDLib's wrong-key error can be fixed by a fresh folder. Any other
/// error (a bad api_id, or a folder another instance holds) cannot, and a
/// wrong match would move a live folder aside and clear its key.
#[must_use]
pub(super) fn is_wrong_key_error(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains(WRONG_KEY_ERROR) && !LOCK_MARKERS.iter().any(|lock| message.contains(lock))
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
    fn a_session_folder_is_new_private_and_hard_to_guess() {
        let root = scratch("session");
        let dir = root.join("thinwire-tdlib-session-x");
        create_private_dir(&dir).expect("new folder");
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let mode = fs::metadata(&dir).expect("meta").mode() & 0o777;
            assert_eq!(mode, 0o700);
        }
        assert!(
            create_private_dir(&dir).is_err(),
            "an existing folder is never reused"
        );
        let a = random_suffix().expect("suffix");
        let b = random_suffix().expect("suffix");
        assert_eq!(a.len(), SESSION_SUFFIX_BYTES * 2);
        assert_ne!(a, b);
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_or_open_folder_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch("symlink");
        let target = root.join("elsewhere");
        fs::create_dir(&target).expect("target");
        let link = root.join("thinwire-tdlib-session-link");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        assert!(create_private_dir(&link).is_err(), "a symlink is refused");
        assert!(verify_private(&link).is_err());
        let open = root.join("open");
        fs::create_dir(&open).expect("open");
        fs::set_permissions(&open, fs::Permissions::from_mode(0o777)).expect("chmod");
        assert!(
            verify_private(&open).is_err(),
            "a folder others can use is refused"
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn only_the_wrong_key_error_triggers_a_fresh_folder() {
        assert!(is_wrong_key_error("Wrong database encryption key"));
        assert!(!is_wrong_key_error("Can't open database"));
        assert!(!is_wrong_key_error("database /x is already in use"));
        assert!(!is_wrong_key_error(
            "Can't lock file \"/a/database/td.binlog\", because it is already in use; check for another program instance running"
        ));
        assert!(!is_wrong_key_error(
            "Wrong database encryption key, and the file lock is held"
        ));
        assert!(!is_wrong_key_error("Valid api_id must be provided"));
        assert!(!is_wrong_key_error("PHONE_NUMBER_INVALID"));
    }
}
