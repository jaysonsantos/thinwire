// SPDX-License-Identifier: AGPL-3.0-only
//! App-data location for the experimental WhatsApp device store.
//!
//! The file is never created inside the git checkout. Callers that open it run
//! on the tokio worker, not the egui thread.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// File name of the device store in the session folder.
const DEVICE_STORE_FILE: &str = "device.sqlite";

/// File name of the lock that one helper process holds on the session
/// folder (ADR 0013).
const SESSION_LOCK_FILE: &str = "helper.lock";

/// A session folder that the helper got with `--session-dir`. It takes the
/// place of the folder under the platform app-data folder.
static SESSION_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Keep the session store in `dir`, not under the platform app-data folder.
/// Call it one time, before the adapter starts. The tests of the helper
/// process use it, so they never touch the session of the user.
///
/// # Errors
///
/// Returns `dir` when a session folder is already set.
pub fn set_session_dir(dir: PathBuf) -> Result<(), PathBuf> {
    SESSION_DIR.set(dir)
}

#[must_use]
pub(crate) fn device_store_file(data_dir: &Path) -> PathBuf {
    data_dir
        .join("thinwire")
        .join("whatsapp")
        .join(DEVICE_STORE_FILE)
}

pub(crate) fn whatsapp_device_store_path() -> Result<PathBuf, ()> {
    if let Some(dir) = SESSION_DIR.get() {
        return Ok(dir.join(DEVICE_STORE_FILE));
    }
    let root = dirs::data_dir().ok_or(())?;
    Ok(device_store_file(&root))
}

/// The lock of one helper process on the session folder. The lock ends when
/// this value is dropped, or when the process ends.
#[derive(Debug)]
pub struct SessionLock {
    _file: std::fs::File,
}

/// Why [`lock_session`] gave no lock. No path and no OS text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionLockError {
    /// The platform has no app-data folder.
    NoDataDir,
    /// Another helper process holds the lock.
    InUse,
    /// The session folder or the lock file could not be made.
    Io(std::io::ErrorKind),
}

impl std::fmt::Display for SessionLockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoDataDir => f.write_str("the platform app-data folder is unavailable"),
            Self::InUse => f.write_str("another helper process uses the WhatsApp session"),
            Self::Io(kind) => write!(f, "the session folder is not usable ({kind})"),
        }
    }
}

impl std::error::Error for SessionLockError {}

/// Take the lock of the session folder for this process.
///
/// Two clients on one device store break it (#98). The app starts one
/// helper at a time, and this lock also stops a helper of a second app
/// process, or one that an app left behind (ADR 0013).
///
/// # Errors
///
/// [`SessionLockError::InUse`] when another process holds the lock.
pub fn lock_session() -> Result<SessionLock, SessionLockError> {
    let store = whatsapp_device_store_path().map_err(|()| SessionLockError::NoDataDir)?;
    let dir = store.parent().ok_or(SessionLockError::NoDataDir)?;
    lock_session_dir(dir)
}

fn lock_session_dir(dir: &Path) -> Result<SessionLock, SessionLockError> {
    let io = |error: std::io::Error| SessionLockError::Io(error.kind());
    prepare_session_dir(dir).map_err(io)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(SESSION_LOCK_FILE))
        .map_err(io)?;
    match file.try_lock() {
        Ok(()) => Ok(SessionLock { _file: file }),
        Err(std::fs::TryLockError::WouldBlock) => Err(SessionLockError::InUse),
        Err(std::fs::TryLockError::Error(error)) => Err(io(error)),
    }
}

pub(crate) fn prepare_session_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Marker file next to the store: the phone revoked this device. It survives
/// a restart, so the next start deletes the revoked store first.
#[cfg_attr(not(any(test, feature = "whatsapp-web")), allow(dead_code))]
const REVOKED_SUFFIX: &str = "-revoked";

/// Files next to the device store: SQLite side files, then the revoked
/// marker. The marker goes last, so a failed delete keeps it.
#[cfg_attr(not(any(test, feature = "whatsapp-web")), allow(dead_code))]
const STORE_SIDE_SUFFIXES: [&str; 4] = ["-wal", "-shm", "-journal", REVOKED_SUFFIX];

/// Path of the revoked marker of `store`.
#[cfg_attr(not(any(test, feature = "whatsapp-web")), allow(dead_code))]
pub(crate) fn revoked_marker(store: &Path) -> PathBuf {
    let mut marker = store.as_os_str().to_owned();
    marker.push(REVOKED_SUFFIX);
    PathBuf::from(marker)
}

/// Delete the device store and its SQLite side files. Missing files are fine.
#[cfg_attr(not(any(test, feature = "whatsapp-web")), allow(dead_code))]
pub(crate) fn remove_device_store(path: &Path) -> std::io::Result<()> {
    let mut targets = vec![path.to_path_buf()];
    for suffix in STORE_SIDE_SUFFIXES {
        let mut side = path.as_os_str().to_owned();
        side.push(suffix);
        targets.push(PathBuf::from(side));
    }
    for target in targets {
        match std::fs::remove_file(&target) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg_attr(not(feature = "whatsapp-web"), allow(dead_code))]
pub(crate) fn restrict_store_file(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if path.exists() {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}
