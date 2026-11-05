//! The audio files that an adapter keeps for the player.
//!
//! Each adapter and account has one folder. The files are plain audio, as
//! the files of TDLib are: the folder is private to the user (0700), and
//! each file is 0600. An adapter empties its folder at start and when its
//! account unlinks. A file name is the handle of the audio, so it holds no
//! chat, no sender, and no text.

use std::io::Write;
use std::path::Path;

use crate::media::AudioPath;

/// The largest audio file an adapter keeps. A voice note is far less.
pub const VOICE_MAX_BYTES: usize = 16 * 1024 * 1024;

/// An audio file was not written. It holds no path and no OS text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreError;

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the audio file was not written")
    }
}

impl std::error::Error for StoreError {}

/// Write `bytes` as the audio `file_id` in `dir` and return its path. The
/// write goes to a temporary name first, so the player never reads a part
/// of a file.
///
/// # Errors
///
/// [`StoreError`] when `bytes` is empty or larger than [`VOICE_MAX_BYTES`],
/// when `file_id` is not positive, or when the folder or the file cannot be
/// written.
pub fn store(dir: &Path, file_id: i32, bytes: &[u8]) -> Result<AudioPath, StoreError> {
    if file_id <= 0 || bytes.is_empty() || bytes.len() > VOICE_MAX_BYTES {
        return Err(StoreError);
    }
    prepare(dir).map_err(|_| StoreError)?;
    let path = dir.join(format!("{file_id}.audio"));
    let part = dir.join(format!("{file_id}.part"));
    write_private(&part, bytes).map_err(|_| {
        let _ = std::fs::remove_file(&part);
        StoreError
    })?;
    std::fs::rename(&part, &path).map_err(|_| {
        let _ = std::fs::remove_file(&part);
        StoreError
    })?;
    // A relative folder is a path of this process only: the app needs the
    // absolute one.
    let absolute = std::path::absolute(&path).map_err(|_| StoreError)?;
    AudioPath::new(absolute.to_string_lossy().into_owned()).ok_or(StoreError)
}

/// Delete every file in `dir`. A missing folder is fine. A file that cannot
/// be deleted stays: the log line has no path.
pub fn clear(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|kind| kind.is_file())
            && std::fs::remove_file(entry.path()).is_err()
        {
            tracing::warn!("an audio file could not be deleted");
        }
    }
}

fn prepare(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .join("thinwire-voice-files-tests")
            .join(format!("{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// The file has the bytes, an absolute path, and private modes. The
    /// name is the handle only. Clear empties the folder.
    #[test]
    fn a_stored_file_is_private_and_clear_removes_it() {
        let dir = folder("store");
        let path = store(&dir, 7, b"OggS-voice").expect("stored");
        assert!(path.as_path().is_absolute());
        assert_eq!(std::fs::read(path.as_path()).expect("read"), b"OggS-voice");
        assert_eq!(
            path.as_path().file_name().and_then(|name| name.to_str()),
            Some("7.audio")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let file = std::fs::metadata(path.as_path()).expect("file");
            assert_eq!(file.permissions().mode() & 0o777, 0o600);
            let folder = std::fs::metadata(&dir).expect("folder");
            assert_eq!(folder.permissions().mode() & 0o777, 0o700);
        }
        assert!(!dir.join("7.part").exists());
        clear(&dir);
        assert!(!path.as_path().exists());
        clear(&dir.join("missing"));
    }

    #[test]
    fn an_empty_a_large_or_a_bad_handle_is_refused() {
        let dir = folder("refuse");
        assert!(store(&dir, 1, b"").is_err());
        assert!(store(&dir, 0, b"x").is_err());
        assert!(store(&dir, -3, b"x").is_err());
        assert!(store(&dir, 1, &vec![0; VOICE_MAX_BYTES + 1]).is_err());
    }
}
