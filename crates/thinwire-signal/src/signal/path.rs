// SPDX-License-Identifier: AGPL-3.0-only
//! App-data location for the local-only Signal session.
//!
//! The directory is never created inside the git checkout. Callers that open
//! it run on the tokio worker, not the egui thread.

use std::path::{Path, PathBuf};

/// File name of the sqlite session inside [`session_dir`].
#[cfg(any(test, feature = "signal-local"))]
pub(crate) const SQLITE_FILE: &str = "session.sqlite";

#[must_use]
#[cfg_attr(not(any(test, feature = "signal-local")), allow(dead_code))]
pub(crate) fn session_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("thinwire").join("signal")
}

/// The sqlite database file. The previous build opened [`session_dir`] itself as a sled database.
#[must_use]
#[cfg(any(test, feature = "signal-local"))]
pub(crate) fn sqlite_store_path(session_dir: &Path) -> PathBuf {
    session_dir.join(SQLITE_FILE)
}

/// `true` when this directory still holds a sled database from the previous store.
#[must_use]
#[cfg(any(test, feature = "signal-local"))]
pub(crate) fn sled_session_present(session_dir: &Path) -> bool {
    session_dir.join("db").is_file() || session_dir.join("conf").is_file()
}

#[cfg_attr(not(feature = "signal-local"), allow(dead_code))]
pub(crate) fn signal_session_path() -> Result<PathBuf, ()> {
    let root = dirs::data_dir().ok_or(())?;
    Ok(session_dir(&root))
}

#[cfg_attr(not(any(test, feature = "signal-local")), allow(dead_code))]
pub(crate) fn prepare_session_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
