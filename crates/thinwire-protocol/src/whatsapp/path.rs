//! App-data location for the experimental WhatsApp device store.
//!
//! The file is never created inside the git checkout. Callers that open it run
//! on the tokio worker, not the egui thread.

use std::path::{Path, PathBuf};

#[must_use]
#[cfg_attr(not(any(test, feature = "whatsapp-web")), allow(dead_code))]
pub(crate) fn device_store_file(data_dir: &Path) -> PathBuf {
    data_dir
        .join("thinwire")
        .join("whatsapp")
        .join("device.sqlite")
}

#[cfg_attr(not(feature = "whatsapp-web"), allow(dead_code))]
pub(crate) fn whatsapp_device_store_path() -> Result<PathBuf, ()> {
    let root = dirs::data_dir().ok_or(())?;
    Ok(device_store_file(&root))
}

#[cfg_attr(not(any(test, feature = "whatsapp-web")), allow(dead_code))]
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
