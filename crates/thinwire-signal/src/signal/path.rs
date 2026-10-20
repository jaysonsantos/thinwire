// SPDX-License-Identifier: AGPL-3.0-only
//! App-data location for the Signal session.
//!
//! The directory is never created inside the git checkout. Callers that open
//! it run on the tokio worker, not the egui thread.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

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

/// File name of the sqlite session inside [`session_dir`].
#[cfg(any(test, feature = "signal-local"))]
pub(crate) const SQLITE_FILE: &str = "session.sqlite";

#[must_use]
pub(crate) fn session_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("thinwire").join("signal")
}

/// The sqlite database file. The previous build opened [`session_dir`] itself as a sled database.
#[must_use]
#[cfg(any(test, feature = "signal-local"))]
pub(crate) fn sqlite_store_path(session_dir: &Path) -> PathBuf {
    session_dir.join(SQLITE_FILE)
}

/// File name of a link that is still in progress. It sits beside [`SQLITE_FILE`]
/// until the link has an account id.
#[cfg(feature = "signal-local")]
pub(crate) const PENDING_FILE: &str = "session.sqlite.pending";

/// Directory name for a previous account's sqlite file. The active store stays
/// [`SQLITE_FILE`].
#[cfg(feature = "signal-local")]
pub(crate) const RETAINED_DIR: &str = "retained";

/// The sqlite file of a link that has not been placed yet.
#[must_use]
#[cfg(feature = "signal-local")]
pub(crate) fn pending_store_path(session_dir: &Path) -> PathBuf {
    session_dir.join(PENDING_FILE)
}

/// `true` when this directory still holds a sled database from the previous store.
#[must_use]
#[cfg(any(test, feature = "signal-local"))]
pub(crate) fn sled_session_present(session_dir: &Path) -> bool {
    session_dir.join("db").is_file() || session_dir.join("conf").is_file()
}

pub(crate) fn signal_session_path() -> Result<PathBuf, ()> {
    if let Some(dir) = SESSION_DIR.get() {
        return Ok(dir.clone());
    }
    let root = dirs::data_dir().ok_or(())?;
    Ok(session_dir(&root))
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
            Self::InUse => f.write_str("another helper process uses the Signal session"),
            Self::Io(kind) => write!(f, "the session folder is not usable ({kind})"),
        }
    }
}

impl std::error::Error for SessionLockError {}

/// Take the lock of the session folder for this process.
///
/// Two clients on one session store break it. The app starts one helper at
/// a time, and this lock also stops a helper of a second app process, or one
/// that an app left behind (ADR 0013).
///
/// # Errors
///
/// [`SessionLockError::InUse`] when another process holds the lock.
pub fn lock_session() -> Result<SessionLock, SessionLockError> {
    let dir = signal_session_path().map_err(|()| SessionLockError::NoDataDir)?;
    lock_session_dir(&dir)
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
