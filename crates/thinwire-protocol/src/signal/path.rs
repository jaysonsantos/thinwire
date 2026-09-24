//! App-data location for the local-only Signal session.
//!
//! The directory is never created inside the git checkout. Callers that open
//! it run on the tokio worker, not the egui thread.

use std::path::{Path, PathBuf};

#[must_use]
#[cfg_attr(not(any(test, feature = "signal-local")), allow(dead_code))]
pub(crate) fn session_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("thinwire").join("signal")
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
