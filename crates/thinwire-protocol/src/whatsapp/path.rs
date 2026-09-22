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
