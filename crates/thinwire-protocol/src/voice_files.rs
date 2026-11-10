//! The audio and video files that an adapter keeps for the player.
//!
//! Each adapter and account has one folder for audio and one for video. The
//! files are plain media, as the files of TDLib are: the folder is private
//! to the user (0700), and each file is 0600. An adapter empties its folders
//! at start and when its account unlinks. A file name is the handle of the
//! media, so it holds no chat, no sender, and no text.
//!
//! An in-app adapter keeps its video clips in the folder of its audio files,
//! with the same rules ([`store_video`]).

use std::io::Write;
use std::path::Path;

use crate::media::AudioPath;

/// The largest audio file an adapter keeps. A voice note is far less.
pub const VOICE_MAX_BYTES: usize = 16 * 1024 * 1024;

/// The largest video file an adapter keeps. WhatsApp and Signal send a
/// video of 100 MB at most.
pub const VIDEO_MAX_BYTES: usize = 100 * 1024 * 1024;

/// A media file was not written. It holds no path and no OS text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreError;

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the media file was not written")
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
    store_as(dir, file_id, bytes, AUDIO_SUFFIX, VOICE_MAX_BYTES)
}

/// Write `bytes` as the video `file_id` in `dir` and return its path. It
/// has the rules of [`store`], with the limit [`VIDEO_MAX_BYTES`].
///
/// # Errors
///
/// [`StoreError`] as [`store`] gives it.
pub fn store_video(dir: &Path, file_id: i32, bytes: &[u8]) -> Result<AudioPath, StoreError> {
    store_as(dir, file_id, bytes, VIDEO_SUFFIX, VIDEO_MAX_BYTES)
}

/// The video `file_id` in `dir`, when an earlier download of this run wrote
/// it. The path is absolute.
#[must_use]
pub fn stored_video(dir: &Path, file_id: i32) -> Option<AudioPath> {
    if file_id <= 0 {
        return None;
    }
    let path = dir.join(format!("{file_id}.{VIDEO_SUFFIX}"));
    let size = std::fs::metadata(&path)
        .ok()
        .filter(|meta| meta.is_file())?;
    if size.len() == 0 {
        return None;
    }
    absolute(&path).ok()
}

/// A video file that a download writes. The file has a temporary name until
/// [`VideoPart::finish`], so the player never reads a part of a file. A
/// part that does not finish is deleted.
#[derive(Debug)]
pub struct VideoPart {
    part: std::path::PathBuf,
    path: std::path::PathBuf,
    done: bool,
}

impl VideoPart {
    /// Start the video `file_id` in `dir`. The file is empty and private
    /// (0600), in a private folder (0700). A download writes into it.
    ///
    /// # Errors
    ///
    /// [`StoreError`] when `file_id` is not positive, or when the folder or
    /// the file cannot be written.
    pub fn begin(dir: &Path, file_id: i32) -> Result<(std::fs::File, Self), StoreError> {
        if file_id <= 0 {
            return Err(StoreError);
        }
        prepare(dir).map_err(|_| StoreError)?;
        let part = dir.join(format!("{file_id}.{PART_SUFFIX}"));
        let file = open_private(&part).map_err(|_| StoreError)?;
        Ok((
            file,
            Self {
                part,
                path: dir.join(format!("{file_id}.{VIDEO_SUFFIX}")),
                done: false,
            },
        ))
    }

    /// The download is complete: give the file its name and return its
    /// absolute path.
    ///
    /// # Errors
    ///
    /// [`StoreError`] when the file is empty or larger than
    /// [`VIDEO_MAX_BYTES`], or when it cannot get its name. The part is
    /// then deleted.
    pub fn finish(mut self, file: std::fs::File) -> Result<AudioPath, StoreError> {
        let size = file.metadata().map_err(|_| StoreError)?.len();
        if size == 0 || size > VIDEO_MAX_BYTES as u64 {
            return Err(StoreError);
        }
        file.sync_all().map_err(|_| StoreError)?;
        drop(file);
        std::fs::rename(&self.part, &self.path).map_err(|_| StoreError)?;
        self.done = true;
        absolute(&self.path)
    }
}

impl Drop for VideoPart {
    fn drop(&mut self) {
        if !self.done {
            let _ = std::fs::remove_file(&self.part);
        }
    }
}

const AUDIO_SUFFIX: &str = "audio";
const VIDEO_SUFFIX: &str = "video";
const PART_SUFFIX: &str = "part";

fn store_as(
    dir: &Path,
    file_id: i32,
    bytes: &[u8],
    suffix: &str,
    max: usize,
) -> Result<AudioPath, StoreError> {
    if file_id <= 0 || bytes.is_empty() || bytes.len() > max {
        return Err(StoreError);
    }
    prepare(dir).map_err(|_| StoreError)?;
    let path = dir.join(format!("{file_id}.{suffix}"));
    // The part name has the kind: an audio file and a video with the same
    // handle can download at the same time in one folder.
    let part = dir.join(format!("{file_id}.{suffix}-{PART_SUFFIX}"));
    write_private(&part, bytes).map_err(|_| {
        let _ = std::fs::remove_file(&part);
        StoreError
    })?;
    std::fs::rename(&part, &path).map_err(|_| {
        let _ = std::fs::remove_file(&part);
        StoreError
    })?;
    absolute(&path)
}

/// A relative folder is a path of this process only: the app needs the
/// absolute one.
fn absolute(path: &Path) -> Result<AudioPath, StoreError> {
    let absolute = std::path::absolute(path).map_err(|_| StoreError)?;
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
            tracing::warn!("a media file could not be deleted");
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

fn open_private(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = open_private(path)?;
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
        assert!(!dir.join("7.audio-part").exists());
        // A clip with the same handle is another file, with the same modes.
        let clip = store_video(&dir, 7, b"clip-bytes").expect("stored");
        assert!(clip.as_path().is_absolute());
        assert_eq!(
            clip.as_path().file_name().and_then(|name| name.to_str()),
            Some("7.video")
        );
        assert_eq!(std::fs::read(path.as_path()).expect("read"), b"OggS-voice");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let file = std::fs::metadata(clip.as_path()).expect("file");
            assert_eq!(file.permissions().mode() & 0o777, 0o600);
        }
        clear(&dir);
        assert!(!path.as_path().exists());
        assert!(!clip.as_path().exists());
        clear(&dir.join("missing"));
    }

    /// A video has its own name and its own limit. It is private, and a
    /// later request finds the file of the earlier download.
    #[test]
    fn a_stored_video_is_private_and_is_found_again() {
        let dir = folder("video");
        assert!(stored_video(&dir, 7).is_none());
        let path = store_video(&dir, 7, b"ftyp-video").expect("stored");
        assert!(path.as_path().is_absolute());
        assert_eq!(
            path.as_path().file_name().and_then(|name| name.to_str()),
            Some("7.video")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let file = std::fs::metadata(path.as_path()).expect("file");
            assert_eq!(file.permissions().mode() & 0o777, 0o600);
            let folder = std::fs::metadata(&dir).expect("folder");
            assert_eq!(folder.permissions().mode() & 0o777, 0o700);
        }
        assert_eq!(stored_video(&dir, 7), Some(path.clone()));
        assert!(stored_video(&dir, 8).is_none());
        assert!(stored_video(&dir, 0).is_none());
        assert!(store_video(&dir, 0, b"x").is_err());
        assert!(store_video(&dir, 1, b"").is_err());
        clear(&dir);
        assert!(stored_video(&dir, 7).is_none());
    }

    /// A download writes into a private part. The file gets its name only
    /// when the download is complete. A part that does not finish, and an
    /// empty one, leave no file.
    #[test]
    fn a_video_part_gets_its_name_only_when_it_is_complete() {
        let dir = folder("part");
        let (mut file, part) = VideoPart::begin(&dir, 9).expect("part");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let meta = std::fs::metadata(dir.join("9.part")).expect("part file");
            assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        }
        file.write_all(b"ftyp-streamed").expect("write");
        assert!(stored_video(&dir, 9).is_none(), "no name before the end");
        let path = part.finish(file).expect("finished");
        assert_eq!(
            std::fs::read(path.as_path()).expect("read"),
            b"ftyp-streamed"
        );
        assert!(!dir.join("9.part").exists());
        assert_eq!(stored_video(&dir, 9), Some(path));

        let (file, part) = VideoPart::begin(&dir, 10).expect("part");
        drop(file);
        drop(part);
        assert!(!dir.join("10.part").exists(), "a dropped part is deleted");

        let (file, part) = VideoPart::begin(&dir, 11).expect("part");
        assert!(part.finish(file).is_err(), "an empty file is not a video");
        assert!(!dir.join("11.part").exists());
        assert!(stored_video(&dir, 11).is_none());
        assert!(VideoPart::begin(&dir, 0).is_err());
        clear(&dir);
    }

    #[test]
    fn an_empty_a_large_or_a_bad_handle_is_refused() {
        let dir = folder("refuse");
        assert!(store(&dir, 1, b"").is_err());
        assert!(store(&dir, 0, b"x").is_err());
        assert!(store(&dir, -3, b"x").is_err());
        assert!(store(&dir, 1, &vec![0; VOICE_MAX_BYTES + 1]).is_err());
        assert!(store_video(&dir, 1, b"").is_err());
        assert!(store_video(&dir, 0, b"x").is_err());
        assert!(store_video(&dir, 1, &vec![0; VIDEO_MAX_BYTES + 1]).is_err());
        // A clip can be larger than an audio file.
        assert!(store_video(&dir, 1, &vec![0; VOICE_MAX_BYTES + 1]).is_ok());
        clear(&dir);
    }
}
