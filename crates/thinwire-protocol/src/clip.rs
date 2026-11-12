//! The download of one audio or video file for the player: from the handle
//! of a row to a private file, and then to the answer for the core.
//!
//! An adapter supplies only the fetch of the bytes. This module owns the
//! other steps, the same for every adapter:
//!
//! - a video that an earlier request of this run wrote answers at once,
//!   when the adapter asks for that ([`Clip::store`], `reuse`);
//! - the write goes to the blocking pool, as a private file
//!   ([`crate::voice_files`]);
//! - the answer is `VoiceReady` or `VoiceFailed` for audio, and
//!   `VideoReady` or `VideoFailed` for video ([`Clip::answer`]).
//!
//! No log line has a path, a chat, or an address.

use std::future::Future;
use std::path::Path;

use crate::adapter::{AdapterEvent, EventTx, ProtocolId};
use crate::media::{AudioPath, VoiceStop};
use crate::voice_files::{self, StoreError};

/// The player that asked for a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClipKind {
    Audio,
    Video,
}

/// One file that a player asked for: the account, the player, and the
/// handle of the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Clip {
    pub protocol: ProtocolId,
    pub kind: ClipKind,
    pub file_id: i32,
}

impl Clip {
    #[must_use]
    pub const fn audio(protocol: ProtocolId, file_id: i32) -> Self {
        Self {
            protocol,
            kind: ClipKind::Audio,
            file_id,
        }
    }

    #[must_use]
    pub const fn video(protocol: ProtocolId, file_id: i32) -> Self {
        Self {
            protocol,
            kind: ClipKind::Video,
            file_id,
        }
    }

    /// The largest file of this kind that an adapter keeps.
    #[must_use]
    pub const fn max_bytes(self) -> usize {
        match self.kind {
            ClipKind::Audio => voice_files::VOICE_MAX_BYTES,
            ClipKind::Video => voice_files::VIDEO_MAX_BYTES,
        }
    }

    /// The event for a file that is ready at `path`.
    #[must_use]
    pub fn ready(self, path: AudioPath) -> AdapterEvent {
        let Self {
            protocol,
            kind,
            file_id,
        } = self;
        match kind {
            ClipKind::Audio => AdapterEvent::VoiceReady {
                protocol,
                file_id,
                path,
            },
            ClipKind::Video => AdapterEvent::VideoReady {
                protocol,
                file_id,
                path,
            },
        }
    }

    /// The event for a file that does not come.
    #[must_use]
    pub fn failed(self) -> AdapterEvent {
        self.failed_with(VoiceStop::Unplayable)
    }

    /// The event for a file that does not come, with the reason for the
    /// audio player. The video player has no reason.
    #[must_use]
    pub fn failed_with(self, stop: VoiceStop) -> AdapterEvent {
        let Self {
            protocol,
            kind,
            file_id,
        } = self;
        match kind {
            ClipKind::Audio => AdapterEvent::VoiceFailed {
                protocol,
                file_id,
                stop,
            },
            ClipKind::Video => AdapterEvent::VideoFailed { protocol, file_id },
        }
    }

    /// The event for the end of a download: ready with the path, or failed.
    #[must_use]
    pub fn event<Failure>(self, stored: Result<AudioPath, Failure>) -> AdapterEvent {
        match stored {
            Ok(path) => self.ready(path),
            Err(_) => {
                tracing::debug!(protocol = ?self.protocol, kind = ?self.kind, "a media file did not download");
                self.failed()
            }
        }
    }

    /// Send the answer of a download to the core.
    pub fn answer<Failure>(self, events: &EventTx, stored: Result<AudioPath, Failure>) {
        let _ = events.send(self.event(stored));
    }

    /// Tell the core that the file does not come.
    pub fn refuse(self, events: &EventTx) {
        let _ = events.send(self.failed());
    }

    /// The private file of this clip in `dir`. With `reuse`, the video file
    /// of an earlier download of this run answers with no fetch: the adapter
    /// sets it only when a handle keeps its file for the whole run. Else
    /// `fetch` gives the bytes, and they are written on the blocking pool.
    ///
    /// # Errors
    ///
    /// [`StoreError`] when the fetch fails, or when the file cannot be
    /// written: no bytes, too many bytes, or a folder that refuses the write.
    pub async fn store<Fetch, Fetched>(
        self,
        dir: &Path,
        reuse: bool,
        fetch: Fetch,
    ) -> Result<AudioPath, StoreError>
    where
        Fetch: FnOnce() -> Fetched,
        Fetched: Future<Output = Result<Vec<u8>, ()>>,
    {
        let file_id = self.file_id;
        if reuse && self.kind == ClipKind::Video {
            let folder = dir.to_path_buf();
            let known =
                tokio::task::spawn_blocking(move || voice_files::stored_video(&folder, file_id))
                    .await
                    .map_err(|_| StoreError)?;
            if let Some(path) = known {
                return Ok(path);
            }
        }
        let bytes = fetch().await.map_err(|()| StoreError)?;
        let folder = dir.to_path_buf();
        let kind = self.kind;
        tokio::task::spawn_blocking(move || match kind {
            ClipKind::Audio => voice_files::store(&folder, file_id, &bytes),
            ClipKind::Video => voice_files::store_video(&folder, file_id, &bytes),
        })
        .await
        .map_err(|_| StoreError)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A new empty folder for one test. The test removes it at its end.
    struct Folder(std::path::PathBuf);

    impl Folder {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("thinwire-clip-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Folder {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const AUDIO: Clip = Clip::audio(ProtocolId::Discord, 7);
    const VIDEO: Clip = Clip::video(ProtocolId::Signal, 7);

    #[test]
    fn each_player_gets_its_own_events() {
        assert!(matches!(
            AUDIO.failed(),
            AdapterEvent::VoiceFailed {
                protocol: ProtocolId::Discord,
                file_id: 7,
                stop: VoiceStop::Unplayable,
            }
        ));
        assert!(matches!(
            VIDEO.failed(),
            AdapterEvent::VideoFailed {
                protocol: ProtocolId::Signal,
                file_id: 7,
            }
        ));
        assert!(matches!(
            VIDEO.event(Err::<AudioPath, ()>(())),
            AdapterEvent::VideoFailed { file_id: 7, .. }
        ));
    }

    #[tokio::test]
    async fn a_fetched_file_is_private_and_answers_ready() {
        let dir = Folder::new("private");
        let folder = dir.path().join("voice");
        let path = AUDIO
            .store(&folder, false, || async { Ok(b"OggS-voice".to_vec()) })
            .await
            .expect("stored");
        assert_eq!(std::fs::read(path.as_path()).expect("file"), b"OggS-voice");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path.as_path())
                .expect("meta")
                .permissions();
            assert_eq!(mode.mode() & 0o777, 0o600);
        }
        assert!(matches!(
            AUDIO.event(Ok::<_, StoreError>(path)),
            AdapterEvent::VoiceReady { file_id: 7, .. }
        ));
    }

    #[tokio::test]
    async fn a_failed_fetch_and_an_empty_file_fail() {
        let dir = Folder::new("failed");
        assert!(
            AUDIO
                .store(dir.path(), false, || async { Err(()) })
                .await
                .is_err()
        );
        assert!(
            VIDEO
                .store(dir.path(), true, || async { Ok(Vec::new()) })
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_stored_video_answers_with_no_second_fetch_only_with_reuse() {
        let dir = Folder::new("reuse");
        let first = VIDEO
            .store(dir.path(), true, || async { Ok(b"ftyp-video".to_vec()) })
            .await
            .expect("stored");
        let again = VIDEO
            .store(dir.path(), true, || async { panic!("no second download") })
            .await
            .expect("known file");
        assert_eq!(first, again);
        // With no reuse, the bytes come again and replace the file.
        VIDEO
            .store(dir.path(), false, || async { Ok(b"ftyp-new".to_vec()) })
            .await
            .expect("stored again");
        assert_eq!(std::fs::read(first.as_path()).expect("file"), b"ftyp-new");
        // The audio of the same handle is another file, and it has no reuse.
        let audio = Clip::audio(ProtocolId::Signal, 7)
            .store(dir.path(), true, || async { Ok(b"OggS".to_vec()) })
            .await
            .expect("audio");
        assert_ne!(audio, first);
    }
}
